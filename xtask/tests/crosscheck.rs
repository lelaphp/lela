//! `xtask crosscheck`, the rendering diff (design §7.1; harness §7, ledger
//! 41): each extension of the bundle's default set rendered by the pinned
//! oracle's `php --re <name>` under the recipe, kept in the store, and
//! held against the engine's rendering, one reading per extension. Every
//! test here goes through the instrument's `run` and reads what it wrote;
//! the verdict on two renderings and the negative control are held
//! beside the lane, in its own tests.
//!
//! The tests that spawn the oracle say so on stderr and return when the
//! pinned binary is not on this machine, unless the machine promised it
//! (#38); the rest run anywhere.

use std::fs;
use std::path::{Path, PathBuf};

use xtask::cli::{Format, Section};
use xtask::crosscheck::{self, Ask};
use xtask::divergence::Registry;
use xtask::manifest::Manifest;
use xtask::oracle::Promise;
use xtask::reading::{Number, Reading, Status, Store};
use xtask::scratch::Tree;

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

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/crosscheck")
}

fn registry() -> Registry {
    Registry::parse("").unwrap()
}

/// A workspace of this test's own: a git repository holding the tracked
/// manifest and pin, and a store of its own for the readings and the
/// oracle's bytes.
struct Scratch {
    root: Tree,
    store: Store,
    manifest: Manifest,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let root = Tree::new(&format!("crosscheck-{name}")).unwrap();
        scratch_workspace(workspace(), &root).unwrap();
        let store = Store::at(root.join("target/harness"));
        Scratch {
            root,
            store,
            manifest: manifest(),
        }
    }

    fn run(&self, ask: &Ask) -> Vec<Reading> {
        let readings =
            crosscheck::run(&self.root, &self.manifest, &registry(), &self.store, ask).unwrap();
        self.store.record(&readings).unwrap();
        readings
    }

    fn one(&self, ask: &Ask) -> Reading {
        let readings = self.run(ask);
        assert_eq!(readings.len(), 1, "{readings:#?}");
        readings.into_iter().next().unwrap()
    }

    /// A directory of renderings standing for the engine's: `ctype.re`
    /// holding the fixture file named.
    fn renderings(&self, fixture: &str) -> PathBuf {
        let dir = self.root.join("renderings");
        fs::create_dir_all(&dir).unwrap();
        fs::copy(fixtures().join(fixture), dir.join("ctype.re")).unwrap();
        dir
    }
}

fn one(name: &str) -> Ask {
    Ask {
        ext: vec![name.to_owned()],
        ..Ask::default()
    }
}

fn count(reading: &Reading, key: &str) -> i64 {
    match reading.numbers.get(key) {
        Some(Number::Int(n)) => *n,
        other => panic!("{key} is {other:?} in {reading:#?}"),
    }
}

/// Design §11 days 1–2: the instrument ships before the engine renders,
/// and every extension of the default set reads red saying the engine
/// rendered nothing, with the oracle's bytes kept for the day it does.
#[test]
fn every_default_extension_reads_fail_and_the_oracles_bytes_are_kept() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new("default");
    let readings = scratch.run(&Ask::default());
    let default: Vec<&str> = scratch
        .manifest
        .crates
        .iter()
        .filter(|c| c.name.starts_with("ext/") && !c.optional())
        .map(|c| c.name.as_str())
        .collect();
    assert!(default.contains(&"ext/ctype"), "{default:?}");
    let scopes: Vec<&str> = readings.iter().map(|r| r.scope.as_str()).collect();
    assert_eq!(scopes, default);
    for reading in &readings {
        assert_eq!(reading.instrument, "crosscheck");
        assert_eq!(reading.status, Status::Fail, "{reading:#?}");
        assert!(
            reading.message.contains("the engine rendered nothing")
                && reading.message.contains("--re"),
            "{}",
            reading.message
        );
        assert!(count(reading, "bytes") > 0, "{reading:#?}");
    }
    let kept = crosscheck::kept(&scratch.store, &scratch.manifest.pin.oracle.md5, "ctype");
    assert_eq!(
        fs::read(&kept).unwrap(),
        fs::read(fixtures().join("oracle.re")).unwrap(),
        "the oracle's bytes at {} are the fixture's",
        kept.display()
    );
    let rendered = xtask::status::render(
        &scratch.manifest,
        &registry(),
        &scratch.store,
        &scratch.root,
        Section::Crosscheck,
        Format::Md,
        None,
        None,
    )
    .unwrap();
    assert!(rendered.contains("| ext/ctype | fail |"), "{rendered}");
}

/// `--ext <name>` is the on-request form (harness §7): one extension, one
/// reading.
#[test]
fn ext_names_one_extension_and_writes_one_reading() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new("one");
    let reading = scratch.one(&one("ctype"));
    assert_eq!(reading.scope, "ext/ctype");
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert!(
        reading.message.contains("php -n --re ctype"),
        "{}",
        reading.message
    );
    assert!(
        scratch
            .store
            .read("crosscheck", "ext/ctype")
            .unwrap()
            .is_some(),
        "the reading is in the store"
    );
}

/// An extension the oracle does not have is a fail carrying php's own
/// words, never a panic.
#[test]
fn an_extension_the_oracle_does_not_have_fails_with_phps_own_words() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new("nosuch");
    let reading = scratch.one(&one("nosuch"));
    assert_eq!(reading.scope, "ext/nosuch");
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert!(
        reading
            .message
            .contains("Extension \"nosuch\" does not exist"),
        "{}",
        reading.message
    );
    assert!(
        !crosscheck::kept(&scratch.store, &scratch.manifest.pin.oracle.md5, "nosuch").exists(),
        "nothing is kept for an extension that rendered nothing"
    );
}

/// The negative control (harness §14 item 4): php's own rendering standing
/// for the engine's is a pass, and the one a byte apart is not.
#[test]
fn the_fixture_through_run_is_a_pass_and_the_one_byte_apart_is_not() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new("fixture");
    let same = Ask {
        renderings: Some(scratch.renderings("oracle.re")),
        ..one("ctype")
    };
    let reading = scratch.one(&same);
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");

    let apart = Ask {
        renderings: Some(scratch.renderings("engine.re")),
        ..one("ctype")
    };
    let reading = scratch.one(&apart);
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert!(
        reading
            .message
            .contains("-    Function [ <internal:ctype> function ctype_alnum ] {")
            && reading
                .message
                .contains("+    Function [ <internal:ctype> function ctype_alnun ] {"),
        "{}",
        reading.message
    );
}

/// An absent oracle is a skip that says why, not a pass and not a fail
/// of the engine.
#[test]
fn an_absent_oracle_is_a_skip_and_not_a_pass() {
    let mut scratch = Scratch::new("absent");
    scratch.manifest.pin.oracle.path = scratch.root.join("nowhere/php");
    let reading = scratch.one(&one("ctype"));
    assert_eq!(reading.status, Status::Skip, "{reading:#?}");
    assert!(
        reading.message.contains("nowhere/php"),
        "{}",
        reading.message
    );
}
