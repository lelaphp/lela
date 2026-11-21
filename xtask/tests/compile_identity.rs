//! `xtask compile-identity`, op-array identity (design §6.1; harness-design.md
//! §4.3, §7): a corpus compiled under the pinned oracle's dump recipe and
//! under the compiler being measured, every file held to byte-identity,
//! and one reading of the corpus. Every test here goes through the
//! instrument's `run` and reads what it wrote; the corpus, the per-file
//! judge and the verdict on two dumps are held beside them, in the lane's
//! own tests.
//!
//! The engine's side is the engine's own binary, `sapi/cli`'s `php`,
//! which honours php's dump flags; the harness links no compiler of its
//! own (harness §8, revision 84), so the tests that hold the
//! compiler build that binary once ([`php`]). The tests that hold the
//! lane's plumbing run the pinned oracle behind a shell file standing
//! where the engine's own binary would: the runner reads the file as an
//! engine, its bytes not being the oracle's, which is how a dump on disk
//! is made measurable. They say so on stderr and return when the pinned
//! binary is not on this machine, unless the machine promised it;
//! the rest run anywhere.
//!
//! **The dump regime.** The recipe both binaries dump under is the pin's
//! `[regime.dump]`, which `oracle.toml` does not carry yet: a person adds
//! it, and until then the lane refuses the run naming it. These tests hand
//! the pin the regime they hold the lane to, as the fixture-pin tests of
//! the other lanes do, so the lane is measured before the pin moves.
#![cfg(tracked)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use xtask::compile_identity::{self, Ask};
use xtask::divergence::Registry;
use xtask::manifest::{Crate, Manifest};
use xtask::oracle::{Promise, Regime};
use xtask::reading::{Number, Reading, Status};
use xtask::scratch::Tree;

mod common {
    pub mod manifest;
    pub mod registry;
    pub mod repo;
}
use common::manifest::{of_rows, pin};
use common::registry::{Row, registry, rows};
use common::repo::scratch_workspace;

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

/// The engine's own binary, `sapi/cli`'s `php`, built once for these
/// tests in the workspace's target: what the lane dumps the engine's side
/// with. A test runs it through [`Scratch::cli`], never from the
/// target itself.
fn php() -> &'static Path {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = Command::new(cargo)
            .args(["build", "--quiet", "-p", "php_sapi_cli", "--bin", "lela"])
            .current_dir(workspace())
            .status()
            .unwrap();
        assert!(status.success(), "cargo could not build lela");
        workspace().join("target/debug/lela")
    })
}

/// The tracked pin's binary, when it is on this machine; a failure where
/// this machine promised the binary.
fn oracle() -> Option<PathBuf> {
    let pin = pin();
    Promise::Oracle
        .or_skip(pin.oracle.file().map(Path::to_path_buf))
        .unwrap()
}

/// The tracked pin's php-src tree, when it is on this machine; a failure
/// where this machine promised the tree.
fn source() -> Option<PathBuf> {
    Promise::Tree.or_skip(pin().source.tree()).unwrap()
}

fn fixtures() -> PathBuf {
    xtask::package_dir().join("fixtures/compile-identity")
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
        ..Regime::default()
    }
}

/// A workspace of this test's own: a git repository holding the tracked
/// manifest, the fixture corpus copied under `corpus/`, and two files
/// standing where the engine's binary would, one handing every call to
/// the pinned oracle and one compiling nothing at all.
struct Scratch {
    root: Tree,
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
        let root = Tree::new(&format!("identity-{name}")).unwrap();
        scratch_workspace(workspace(), &root);
        let corpus = root.join("corpus");
        fs::create_dir_all(&corpus).unwrap();
        let fixtures = xtask::package_dir().join("fixtures/compile-identity/corpus");
        for entry in fs::read_dir(&fixtures).unwrap() {
            let file = entry.unwrap().path();
            let name = file.file_name().unwrap().to_string_lossy().into_owned();
            if only.contains(&name.as_str()) {
                fs::copy(&file, corpus.join(&name)).unwrap();
            }
        }
        let mut manifest = of_rows([Crate::named("php_compile")]);
        manifest
            .pin
            .as_mut()
            .unwrap()
            .regime
            .insert(compile_identity::REGIME.to_owned(), dump_regime());
        Scratch {
            root,
            corpus,
            manifest,
        }
    }

    /// The pin's `[regime.dump]` carrying `args` beside its ini.
    fn dump_with_args(&mut self, args: &[&str]) {
        self.manifest
            .pin
            .as_mut()
            .unwrap()
            .regime
            .get_mut(compile_identity::REGIME)
            .unwrap()
            .args = args.iter().map(|&arg| arg.to_owned()).collect();
    }

    /// A file standing where the engine's binary would: `body` is the
    /// shell it runs, which either hands the call to the pinned oracle or
    /// compiles nothing.
    fn binary(&self, name: &str, body: &str) -> PathBuf {
        let path = self.root.join("engine").join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        xtask::scratch::executable(&path).unwrap();
        path
    }

    /// The engine's binary [`php`] built, standing alone in a directory of
    /// the scratch's own as links to it and to its dep-info. The lane
    /// holds a binary to the tree by that dep-info, and every other host
    /// standing beside it to its own; these tests build `php`
    /// alone, so in the target a `php-cgi` an earlier build left behind an
    /// engine merge failed them for a build they never asked for. Here
    /// `php` is held to its own sources as cargo relinks it, and to no
    /// other host's.
    fn cli(&self) -> PathBuf {
        let dir = self.root.join("cli");
        fs::create_dir_all(&dir).unwrap();
        let php = php();
        let mut info = php.as_os_str().to_owned();
        info.push(".d");
        for (link, to) in [("php", php.as_os_str()), ("php.d", info.as_os_str())] {
            fs::remove_file(dir.join(link)).ok();
            std::os::unix::fs::symlink(to, dir.join(link)).unwrap();
        }
        dir.join("php")
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
        let readings = compile_identity::run(&self.root, &self.manifest, registry, ask)
            .unwrap()
            .readings;
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

/// The floor: the files of the fixture corpus the linked compiler has
/// come to byte for byte, which is both of the two php dumps an op array
/// for (6043007). It is a floor and not the reach: `php_compile`
/// advancing moves `identical` up and this number is raised beside such a
/// commit or not at all, where the constant it replaces was a
/// number the next advance made wrong (harness-design.md §7, ledger 49).
const REACHED: i64 = 2;

/// The reach of one corpus reading against the floor, the number named
/// on both sides: a file the compiler had come to that no longer dumps
/// identical is the regression this holds, and a file it has newly come
/// to is not.
fn below_the_floor(reading: &Reading) -> Result<(), String> {
    let identical = count(reading, "identical");
    if identical >= REACHED {
        Ok(())
    } else {
        Err(format!(
            "below the floor: {identical} of the corpus's files dump byte-identical \
             to the oracle's where {REACHED} did"
        ))
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

/// Design §6.1: the lane compiles in this process, through `php_bundle`,
/// so it waits on no engine binary. With none named and none built, the
/// rung below the binary is the linked compiler, and the reading is the
/// engine's own, with the denominator stated: red while any file php
/// dumped an op array for differs, and the count of the files the
/// compiler has come to byte for byte. A skip for want of a binary would
/// say nothing about how far the compiler has to come. The reach is read
/// off the reading and not pinned here: `plain.php`, one
/// assignment and an `echo`, reached identity with `Unit::dump` in php's
/// format (0c80b02) and `INIT_FCALL`'s op1 (535cdd6); `identity.phpt`
/// calls a user function and reached it with the op array a function body
/// compiles to (6043007), and the commit that pinned the reach at one of
/// the two left this test red from the second onwards. What is held is
/// the corpus's own shape, the denominator and the oracle's two skips,
/// both of them this crate's fixtures and the pin's; the message from the
/// counts; and `REACHED`, the floor a file the compiler has come to may
/// not fall back through (harness-design.md §7, ledger 49). The red
/// branch of the same rung is held by
/// `a_compiler_that_emits_nothing_is_red_on_every_file_with_the_denominator`,
/// the compiler that dumps nothing at all, and is exercised there while
/// the corpus stands green here.
#[test]
fn the_engines_binary_dumps_the_corpus_at_its_reach() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new("engine");
    let reading = scratch.run(&registry(""), &scratch.cli());
    assert_eq!(reading.scope, "corpus");
    // The same four files the binary of a compiler that emits nothing is
    // held to: two php dumps an op array for and two it dumps nothing
    // for. Both counts are the corpus's and the oracle's, so neither
    // moves with the compiler.
    assert_eq!(count(&reading, "files"), 4);
    assert_eq!(count(&reading, "skipped"), 2);
    let identical = count(&reading, "identical");
    let differ = count(&reading, "differ");
    assert_eq!(identical + differ, 2, "{reading:#?}");
    if let Err(why) = below_the_floor(&reading) {
        panic!("{why}: {reading:#?}");
    }
    // The verdict is the differing files, so the message names each of
    // them and the reading is red while there is one; with none it says
    // the denominator, the two php dumped nothing for skipped beside it.
    assert_eq!(
        reading.status,
        if differ == 0 {
            Status::Pass
        } else {
            Status::Fail
        },
        "{reading:#?}"
    );
    if differ == 0 {
        assert!(
            reading.message.contains(&format!(
                "{identical} of 4 files dump byte-identical to the oracle's"
            )) && reading.message.contains("2 skipped")
                && !reading.message.contains("identity.phpt")
                && !reading.message.contains("plain.php"),
            "{}",
            reading.message
        );
    } else {
        assert!(
            reading
                .message
                .contains(&format!("{differ} of 4 files differ")),
            "{}",
            reading.message
        );
    }
}

/// The floor is a sensor and not a comment (harness-design.md §7,
/// ledger 49): the reading of a compiler that emits nothing is below it
/// and the failure names both numbers, and the engine's binary's is not
/// below it. So a commit that carries `php_compile` further through the
/// corpus leaves the harness's own suite green and one that carries it
/// back does not.
#[test]
fn a_reach_under_the_floor_is_named_and_the_engines_is_not() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new("floor");
    let nothing = scratch.run(&registry(""), &scratch.emits_nothing());
    let why = below_the_floor(&nothing).unwrap_err();
    assert!(
        why.contains("0 of") && why.contains(&REACHED.to_string()),
        "{why}"
    );
    let engine = scratch.run(&registry(""), &scratch.cli());
    assert_eq!(below_the_floor(&engine), Ok(()), "{engine:#?}");
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
    let accepted = rows(&[
        Row::accepted("compile:corpus/identity.phpt"),
        Row::accepted("compile:corpus/plain.php"),
    ]);
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
    manifest.pin.as_mut().unwrap().oracle.path = scratch.root.join("nowhere/php");
    let ask = Ask {
        corpus: Some(scratch.corpus.clone()),
        engine: Some(scratch.binary("php", "exit 0")),
        ..Ask::default()
    };
    let readings = compile_identity::run(&scratch.root, &manifest, &registry(""), &ask)
        .unwrap()
        .readings;
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
    manifest.pin.as_mut().unwrap().oracle.digest = xtask::oracle::Digest::Md5("0".repeat(32));
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
    manifest
        .pin
        .as_mut()
        .unwrap()
        .regime
        .remove(compile_identity::REGIME);
    let ask = Ask {
        corpus: Some(scratch.corpus.clone()),
        ..Ask::default()
    };
    let refused = compile_identity::run(&scratch.root, &manifest, &registry(""), &ask).unwrap_err();
    assert!(format!("{refused:#}").contains("dump"), "{refused:#}");
}

/// A regime's `args` reach both binaries' command lines:
/// under a `dump` regime that carries `-e`, php dumps with extended info,
/// so the binary that hands php every argument it was given is identical
/// on every file, and the one that hands it all but `-e` differs on each
/// file php dumped an op array for. The difference is the argument
/// reaching the oracle's side, and the identity is it reaching the
/// engine's.
#[test]
fn a_regimes_args_reach_both_binaries() {
    let Some(php) = oracle() else {
        return;
    };
    let mut scratch = Scratch::new("args");
    scratch.dump_with_args(&["-e"]);
    let same = scratch.run(&registry(""), &scratch.same_as_php());
    assert_eq!(same.status, Status::Pass, "{same:#?}");
    assert_eq!(count(&same, "identical"), 2);
    let dropped = scratch.binary(
        "drops-e",
        &format!(
            "for arg in \"$@\"; do shift; [ \"$arg\" = -e ] || set -- \"$@\" \"$arg\"; done\nexec \"{}\" \"$@\"",
            php.display()
        ),
    );
    let without = scratch.run(&registry(""), &dropped);
    assert_eq!(without.status, Status::Fail, "{without:#?}");
    assert_eq!(count(&without, "differ"), 2, "{without:#?}");
}

/// With no engine binary built, and none named, nothing dumps the
/// engine's side: the harness links no compiler to fall back to,
/// so every file is found and none is run, and the run says why.
#[test]
fn with_no_engine_binary_nothing_is_dumped_and_the_reading_says_so() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new("no-engine");
    let reading = scratch.ask(
        &registry(""),
        &Ask {
            corpus: Some(scratch.corpus.clone()),
            ..Ask::default()
        },
    );
    assert_eq!(count(&reading, "files"), 4, "{reading:#?}");
    assert_ne!(reading.status, Status::Pass, "{reading:#?}");
    assert!(
        !reading.message.contains("byte-identical"),
        "{}",
        reading.message
    );
}

/// The program `--regime extended-info` is held to beside the fixture
/// corpus: a statement of each kind php puts an extended opline before,
/// at or around.
const EXTENDED: &str = "<?php\nfunction f($a) { return $a + 1; }\n\
    if ($x) { f(1); } elseif ($y) { echo 2; } else { echo 3; }\n\
    for ($i = 0; $i < 2; $i++) { echo $i; }\ninclude 'x.php';\n\
    $v = 1 |> f(...);\necho 1, 2;\nunset($a, $b);\n\
    $o = new ArrayObject([]);\nclass C { public $p; public $q, $r; function m() { return 1; } }\n\
    enum E { case A; case B; }\n$g = fn($x) => $x;\n";

/// A second regime beside `dump`: `--regime extended-info`, the
/// tracked pin's row of php's `-e`, is handed to both sides after `dump`,
/// so the oracle and the engine's binary each dump with extended info,
/// php's `-e` on both command lines, and the reading keeps a scope of its
/// own beside the plain run's. The plain run of the same
/// corpus is its control: what differs between the two is the regime,
/// and both are byte-identical.
#[test]
fn a_second_regime_holds_the_engines_binary_under_extended_info() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::of("extended", &["identity.phpt", "plain.php"]);
    fs::write(scratch.corpus.join("extended.php"), EXTENDED).unwrap();
    let engine = scratch.cli();
    let asked = |regime: Option<&str>| {
        scratch.ask(
            &registry(""),
            &Ask {
                corpus: Some(scratch.corpus.clone()),
                engine: Some(engine.clone()),
                regime: regime.map(str::to_owned),
                ..Ask::default()
            },
        )
    };
    let extended = asked(Some("extended-info"));
    assert_eq!(extended.scope, "corpus@regime=extended-info");
    assert_eq!(extended.status, Status::Pass, "{extended:#?}");
    assert_eq!(count(&extended, "identical"), 3, "{extended:#?}");
    let plain = asked(None);
    assert_eq!(plain.scope, "corpus");
    assert_eq!(plain.status, Status::Pass, "{plain:#?}");
    assert_eq!(count(&plain, "identical"), 3, "{plain:#?}");
}

/// The regime a run names is the pin's: one it does not carry refuses
/// the run before anything is dumped, naming the flag.
#[test]
fn a_regime_the_pin_does_not_carry_refuses_the_run() {
    let scratch = Scratch::new("unknown-regime");
    let refused = compile_identity::run(
        &scratch.root,
        &scratch.manifest,
        &registry(""),
        &Ask {
            corpus: Some(scratch.corpus.clone()),
            regime: Some("no-such-regime".to_owned()),
            ..Ask::default()
        },
    )
    .unwrap_err();
    let refused = format!("{refused:#}");
    assert!(
        refused.contains("--regime no-such-regime") && refused.contains("extended-info"),
        "{refused}"
    );
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
    let readings = compile_identity::run(&scratch.root, &scratch.manifest, &registry(""), &same)
        .unwrap()
        .readings;
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

/// A binary under measurement whose dump of one file runs past the
/// recipe's limit, or dies on a signal, fails that file and the lane goes
/// on to the next: of three files, the one it hangs on and
/// the one it dies on differ, and the one after both is still dumped and
/// identical. The limit is ten seconds, in the test's own pin, against
/// the thirty the hang holds for: the files meant to pass, and the pinned
/// php's probe of its own `PHP_BINARY`, need finish only inside it, a margin
/// a loaded machine's process start does not eat (at one second they did
/// not, two whole-crate runs in four at load 19-35).
#[test]
fn a_dump_past_the_limit_or_ended_by_a_signal_fails_that_file_and_the_rest_are_judged() {
    let Some(php) = oracle() else {
        return;
    };
    let mut scratch = Scratch::of("past-limit", &["identity.phpt", "plain.php"]);
    fs::write(scratch.corpus.join("zz.php"), "<?php\necho 1;\n").unwrap();
    scratch
        .manifest
        .pin
        .as_mut()
        .unwrap()
        .recipe
        .limits
        .timeout_s = 10;
    let engine = scratch.binary(
        "php",
        &format!(
            "case \"$*\" in\n  *identity.php*) sleep 30 ;;\n  *plain.php*) kill -SEGV $$ ;;\nesac\nexec \"{}\" \"$@\"",
            php.display()
        ),
    );
    let reading = scratch.run(&registry(""), &engine);
    assert_eq!(count(&reading, "files"), 3, "{reading:#?}");
    assert_eq!(count(&reading, "differ"), 2, "{reading:#?}");
    assert_eq!(count(&reading, "borked"), 0, "{reading:#?}");
    assert_eq!(count(&reading, "identical"), 1, "{reading:#?}");
    assert!(
        reading.message.contains("identity.phpt") && reading.message.contains("plain.php"),
        "{}",
        reading.message
    );
}

/// The reading names the binary that dumped the corpus, its path and the
/// md5 of its bytes, as `phpt`'s does (`Runner::stamp`), so two
/// readings over one tree under two binaries are told apart; where no
/// binary dumped, the engine's absent, the reading names none.
#[test]
fn the_reading_names_the_binary_that_dumped_it_and_none_where_none_did() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new("stamp");
    let engine = scratch.same_as_php();
    let reading = scratch.run(&registry(""), &engine);
    let md5 = format!("{:x}", md5::compute(fs::read(&engine).unwrap()));
    assert!(
        reading
            .message
            .ends_with(&format!("; dumped by {} md5 {md5}", engine.display())),
        "{}",
        reading.message
    );

    let absent = scratch.ask(
        &registry(""),
        &Ask {
            corpus: Some(scratch.corpus.clone()),
            ..Ask::default()
        },
    );
    assert!(!absent.message.contains("dumped by"), "{}", absent.message);
}
