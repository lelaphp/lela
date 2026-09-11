//! `xtask compile-identity`, op-array identity (design §6.1; harness-design.md
//! §4.3, §7): a corpus compiled under the pinned oracle's dump recipe and
//! under a binary standing where the engine will, every file held to
//! byte-identity, and one reading of the corpus. Every test here goes
//! through the instrument's `run` and reads what it wrote; the corpus, the
//! per-file judge and the verdict on two dumps are held beside them, in
//! the lane's own tests.
//!
//! The tests that spawn php run the pinned oracle behind a shell file
//! standing where the engine's binary would, since the engine that will be
//! measured does not exist yet: the runner reads the file as an engine,
//! its bytes not being the oracle's. They say so on stderr and return when
//! the pinned binary is not on this machine, unless the machine promised
//! it (#38); the rest run anywhere.
//!
//! **The dump regime.** The recipe both binaries dump under is the pin's
//! `[regime.dump]`, which `oracle.toml` does not carry yet: a person adds
//! it, and until then the lane refuses the run naming it. These tests hand
//! the pin the regime they hold the lane to, as the fixture-pin tests of
//! the other lanes do, so the lane is measured before the pin moves.

use std::fs;
use std::path::{Path, PathBuf};

use xtask::compile_identity::{self, Ask};
use xtask::divergence::Registry;
use xtask::manifest::Manifest;
use xtask::oracle::{Promise, Regime};
use xtask::reading::{Number, Reading, Status};

mod common;
use common::scratch_workspace;

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

fn manifest() -> Manifest {
    Manifest::load(workspace()).unwrap()
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

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/compile-identity")
}

fn registry(text: &str) -> Registry {
    Registry::parse(text).unwrap()
}

/// php's own dump recipe (design §6.1) as a `[regime.dump]` of the pin:
/// what a person's `oracle.toml` carries, handed to the pin here so the
/// lane can be held to it before the pin moves.
fn dump_regime() -> Regime {
    Regime {
        ini: vec![
            "opcache.enable_cli=1".to_owned(),
            "opcache.opt_debug_level=0x10000".to_owned(),
        ],
    }
}

/// A workspace of this test's own: a git repository holding the tracked
/// manifest, the fixture corpus copied under `corpus/`, and two files
/// standing where the engine's binary would, one handing every call to
/// the pinned oracle and one compiling nothing at all.
struct Scratch {
    root: PathBuf,
    corpus: PathBuf,
    manifest: Manifest,
}

impl Scratch {
    /// The whole fixture corpus.
    fn new(name: &str) -> Scratch {
        Scratch::of(
            name,
            &[
                "identity.phpt",
                "plain.php",
                "redirect.phpt",
                "unparsable.phpt",
            ],
        )
    }

    /// The files of the fixture corpus named, and an empty corpus for
    /// none: what a test that lays its own files down starts from.
    fn of(name: &str, only: &[&str]) -> Scratch {
        let root =
            std::env::temp_dir().join(format!("xtask-identity-{name}-{}", std::process::id()));
        scratch_workspace(workspace(), &root).unwrap();
        let corpus = root.join("corpus");
        fs::create_dir_all(&corpus).unwrap();
        let fixtures =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/compile-identity/corpus");
        for entry in fs::read_dir(&fixtures).unwrap() {
            let file = entry.unwrap().path();
            let name = file.file_name().unwrap().to_string_lossy().into_owned();
            if only.contains(&name.as_str()) {
                fs::copy(&file, corpus.join(&name)).unwrap();
            }
        }
        let mut manifest = Manifest::named("php_compile");
        manifest.pin = self::manifest().pin;
        manifest
            .pin
            .regime
            .insert(compile_identity::REGIME.to_owned(), dump_regime());
        Scratch {
            root,
            corpus,
            manifest,
        }
    }

    /// A file standing where the engine's binary would: `body` is the
    /// shell it runs, which either hands the call to the pinned oracle or
    /// compiles nothing.
    fn binary(&self, name: &str, body: &str) -> PathBuf {
        let path = self.root.join("engine").join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    /// The binary that dumps what php dumps: the lane's own control, since
    /// the compiler it will measure does not exist yet.
    fn same_as_php(&self) -> PathBuf {
        self.binary(
            "php",
            &format!("exec \"{}\" \"$@\"", oracle().unwrap().display()),
        )
    }

    /// A binary that prints one file as its dump, whatever it is given:
    /// what makes a dump on disk measurable as an engine's.
    fn prints(&self, name: &str, dump: &Path) -> PathBuf {
        self.binary(name, &format!("cat \"{}\" >&2", dump.display()))
    }

    /// The binary of a compiler that emits nothing: the day-2 engine of
    /// design §11, which every file of the corpus must read red against.
    fn emits_nothing(&self) -> PathBuf {
        self.binary("empty", "exit 0")
    }

    /// The one reading of the fixture corpus, dumped against `engine`.
    fn run(&self, registry: &Registry, engine: &Path) -> Reading {
        self.ask(
            registry,
            &Ask {
                corpus: Some(self.corpus.clone()),
                engine: Some(engine.to_path_buf()),
                ..Ask::default()
            },
        )
    }

    fn ask(&self, registry: &Registry, ask: &Ask) -> Reading {
        let readings = compile_identity::run(&self.root, &self.manifest, registry, ask).unwrap();
        assert_eq!(readings.len(), 1, "{readings:#?}");
        readings.into_iter().next().unwrap()
    }
}

fn count(reading: &Reading, key: &str) -> i64 {
    match reading.numbers.get(key) {
        Some(Number::Int(n)) => *n,
        other => panic!("{key} is {other:?} in {reading:#?}"),
    }
}

/// Design §11 row 1–2: the instrument ships before the compiler it
/// measures, and against a compiler that emits nothing every file of the
/// corpus reads red. The reading states the denominator, which is what
/// says how far the compiler has to come.
#[test]
fn a_compiler_that_emits_nothing_is_red_on_every_file_with_the_denominator() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new("nothing");
    let reading = scratch.run(&registry(""), &scratch.emits_nothing());
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(reading.instrument, "compile-identity");
    assert_eq!(reading.scope, "corpus");
    // The corpus holds four files: one redirects elsewhere and holds no
    // program, one php cannot compile and so dumps nothing, and the two
    // that are dumped both differ. A file php compiled nothing from is
    // not red, and not green either.
    assert_eq!(count(&reading, "files"), 4);
    assert_eq!(count(&reading, "differ"), 2);
    assert_eq!(count(&reading, "identical"), 0);
    assert_eq!(count(&reading, "skipped"), 2);
    assert!(
        reading.message.contains("2 of 4 files differ")
            && reading.message.contains("identity.phpt")
            && reading.message.contains("plain.php"),
        "{}",
        reading.message
    );
}

/// The lane's plumbing, measured: php's dump against php's dump is
/// identical on every file the corpus holds a program for, so a red
/// reading is the compiler's distance and never the recipe's.
#[test]
fn the_oracles_own_dump_is_identical_on_every_file() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new("identical");
    let reading = scratch.run(&registry(""), &scratch.same_as_php());
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert_eq!(count(&reading, "identical"), 2);
    assert_eq!(count(&reading, "differ"), 0);
    assert_eq!(count(&reading, "skipped"), 2);
    assert_eq!(count(&reading, "run"), 4);
}

/// The pinned oracle named as the binary under measurement is the lane's
/// control: a reading of its own, and a skip whatever the files say
/// (harness §4.3).
#[test]
fn the_pinned_oracle_as_the_binary_is_a_control_and_not_a_reading_of_the_engine() {
    let Some(pinned) = oracle() else {
        return;
    };
    let scratch = Scratch::new("control");
    let reading = scratch.run(&registry(""), &pinned);
    assert_eq!(reading.status, Status::Skip, "{reading:#?}");
    assert_eq!(reading.scope, "corpus@control");
    assert!(
        reading.message.contains("control of the lane"),
        "{}",
        reading.message
    );
}

/// The same difference under an accepted row of kind `compile`: the
/// registry is the one door a difference is accepted through, and the
/// reading is green with the row counted (harness §4.3).
#[test]
fn a_difference_an_accepted_row_names_is_met() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new("accepted");
    let accepted = registry(
        r#"
[[divergence]]
id = "compile:corpus/identity.phpt"
kind = "compile"
reason = "A person's sentence about this file."
status = "accepted"
since = "2026-09-11"
[[divergence]]
id = "compile:corpus/plain.php"
kind = "compile"
reason = "A person's sentence about that one."
status = "accepted"
since = "2026-09-11"
"#,
    );
    let reading = scratch.run(&accepted, &scratch.emits_nothing());
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert_eq!(count(&reading, "diverged"), 2);
    assert_eq!(count(&reading, "differ"), 0);
}

/// A pinned binary that is not on this machine is a skip that says so,
/// and never a pass: nothing was dumped, and the files it did not dump
/// are the denominator.
#[test]
fn an_absent_oracle_is_a_skip_and_not_a_pass() {
    let scratch = Scratch::new("absent");
    let mut manifest = scratch.manifest.clone();
    manifest.pin.oracle.path = scratch.root.join("nowhere/php");
    let ask = Ask {
        corpus: Some(scratch.corpus.clone()),
        engine: Some(scratch.binary("php", "exit 0")),
        ..Ask::default()
    };
    let readings = compile_identity::run(&scratch.root, &manifest, &registry(""), &ask).unwrap();
    let reading = &readings[0];
    assert_eq!(reading.status, Status::Skip, "{reading:#?}");
    assert_eq!(count(reading, "files"), 4);
    assert_eq!(count(reading, "run"), 0);
    assert!(
        reading.message.contains("no file of 4 dumped") && reading.message.contains("nowhere/php"),
        "{}",
        reading.message
    );
}

/// A binary at the pin's path whose digest is not the pin's refuses the
/// whole run, before the first file rather than at each (harness §4.3).
#[test]
fn a_digest_that_is_not_the_pins_refuses_the_run() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new("digest");
    let mut manifest = scratch.manifest.clone();
    manifest.pin.oracle.md5 = "0".repeat(32);
    let ask = Ask {
        corpus: Some(scratch.corpus.clone()),
        engine: Some(scratch.binary("php", "exit 0")),
        ..Ask::default()
    };
    let refused = compile_identity::run(&scratch.root, &manifest, &registry(""), &ask).unwrap_err();
    assert!(
        format!("{refused:#}").contains("the pin says"),
        "{refused:#}"
    );
}

/// The recipe is the pin's: a pin without the `dump` regime refuses the
/// run naming it, rather than the lane stating a recipe of its own.
#[test]
fn a_pin_without_the_dump_regime_refuses_the_run() {
    let scratch = Scratch::new("regime");
    let mut manifest = scratch.manifest.clone();
    manifest.pin.regime.remove(compile_identity::REGIME);
    let ask = Ask {
        corpus: Some(scratch.corpus.clone()),
        ..Ask::default()
    };
    let refused = compile_identity::run(&scratch.root, &manifest, &registry(""), &ask).unwrap_err();
    assert!(format!("{refused:#}").contains("dump"), "{refused:#}");
}

/// One file, on request (harness §7): the verdict is printed, a difference
/// is the exit, and nothing is recorded, since one file is no measurement
/// of a corpus.
#[test]
fn one_file_on_request_records_nothing_and_a_difference_is_the_exit() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new("one");
    let file = scratch.corpus.join("identity.phpt");
    let same = Ask {
        file: Some(file.clone()),
        engine: Some(scratch.same_as_php()),
        ..Ask::default()
    };
    let readings =
        compile_identity::run(&scratch.root, &scratch.manifest, &registry(""), &same).unwrap();
    assert!(readings.is_empty(), "{readings:#?}");
    let differs = Ask {
        file: Some(file),
        engine: Some(scratch.emits_nothing()),
        ..Ask::default()
    };
    let refused = compile_identity::run(&scratch.root, &scratch.manifest, &registry(""), &differs)
        .unwrap_err();
    assert!(
        format!("{refused:#}").contains("identity.phpt"),
        "{refused:#}"
    );
}

/// The negative control the fixture is (harness §12 item 4), end to end:
/// `oracle.dump` is php's own bytes for the fixture's program, so a binary
/// that prints it is identical; `engine.dump`, which differs from it by
/// one line, is not a pass. The first half is what keeps the fixture from
/// drifting away from what php prints.
#[test]
fn the_fixture_is_the_oracles_own_dump_and_the_one_line_apart_is_not_a_pass() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::of("fixture", &["identity.phpt"]);
    let same = scratch.prints("same", &fixtures().join("oracle.dump"));
    let reading = scratch.run(&registry(""), &same);
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert_eq!(count(&reading, "identical"), 1);

    let apart = scratch.prints("apart", &fixtures().join("engine.dump"));
    let reading = scratch.run(&registry(""), &apart);
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(count(&reading, "differ"), 1);
    assert!(
        reading.message.contains("1 of 1 files differ"),
        "{}",
        reading.message
    );
}

/// The lane over the corpus it will be run on: the first files of
/// `Zend/tests` of the pinned tree, whose programs are what php-src
/// itself writes rather than what a fixture does. php's dump of them
/// against php's is identical, so what a red reading will say later is
/// the compiler's distance and never the corpus's shape.
#[test]
fn real_corpus_files_dump_identical_under_the_oracle_against_itself() {
    let (Some(tree), Some(_)) = (source(), oracle()) else {
        return;
    };
    let scratch = Scratch::of("real", &[]);
    let mut found: Vec<PathBuf> = fs::read_dir(tree.join("Zend/tests"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "phpt"))
        .collect();
    found.sort();
    assert!(found.len() > 20, "the pinned Zend/tests holds the corpus");
    for file in found.iter().take(20) {
        fs::copy(file, scratch.corpus.join(file.file_name().unwrap())).unwrap();
    }
    let reading = scratch.run(&registry(""), &scratch.same_as_php());
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert_eq!(count(&reading, "files"), 20);
    assert_eq!(count(&reading, "differ"), 0);
}

/// A file of the pinned corpus, on request, is keyed by where it stands
/// in the corpus, so a row a person writes from a look at one file is the
/// row a run of the whole corpus would want.
#[test]
fn one_file_of_the_pinned_tree_is_keyed_by_the_corpus_it_stands_in() {
    let (Some(tree), Some(_)) = (source(), oracle()) else {
        return;
    };
    let mut found: Vec<PathBuf> = fs::read_dir(tree.join("Zend/tests"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "phpt"))
        .collect();
    found.sort();
    let file = found.first().unwrap().clone();
    let inside = file.strip_prefix(&tree).unwrap().display().to_string();
    let scratch = Scratch::of("one-of-the-tree", &[]);
    // The engine dumps nothing, so the file differs and the refusal names
    // it: what it is named is what a registry row would be keyed by.
    let ask = Ask {
        file: Some(file),
        engine: Some(scratch.emits_nothing()),
        ..Ask::default()
    };
    let refused =
        compile_identity::run(&scratch.root, &scratch.manifest, &registry(""), &ask).unwrap_err();
    assert!(format!("{refused:#}").contains(&inside), "{refused:#}");
}
