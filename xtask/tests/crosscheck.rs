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
use xtask::oracle::{self, Promise};
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

/// The extensions the engine renders as the oracle does, so far: ctype
/// (#271) and json (#272). The row for the next one ported is added
/// here with the commit that makes it pass.
const PORTED: [&str; 2] = ["ext/ctype", "ext/json"];

/// Design §11 days 1–2, the two halves met (#269): every extension of
/// the default set is rendered by the oracle and by the linked engine,
/// the ported ones read pass, and every other reads fail naming the
/// engine's rendering as the cause, the first differing line or the
/// engine's own words for rendering nothing; the oracle's bytes are kept
/// for each.
#[test]
fn the_ported_extensions_read_pass_and_every_other_default_extension_reads_fail_naming_the_engines_rendering()
 {
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
        assert!(reading.message.contains("--re"), "{}", reading.message);
        assert!(count(reading, "bytes") > 0, "{reading:#?}");
        if PORTED.contains(&reading.scope.as_str()) {
            assert_eq!(reading.status, Status::Pass, "{reading:#?}");
            assert!(
                reading
                    .message
                    .contains("the engine's rendering is the oracle's"),
                "{}",
                reading.message
            );
            continue;
        }
        assert_eq!(reading.status, Status::Fail, "{reading:#?}");
        assert!(
            reading
                .message
                .contains("the engine's rendering differs from the oracle's at line")
                || reading.message.contains("the engine rendered nothing"),
            "{}",
            reading.message
        );
    }
    let kept = crosscheck::kept(&scratch.store, &scratch.manifest.pin.oracle.md5, "ctype");
    // The fixture is php's own bytes; what is kept is what a run compares,
    // the fixture under the recipe's normalisation, its load number the
    // pin's token (#317).
    assert_eq!(
        fs::read(&kept).unwrap(),
        oracle::normalise(
            &scratch.manifest.pin.recipe.normalise,
            fs::read(fixtures().join("oracle.re")).unwrap()
        ),
        "the oracle's bytes at {} are the fixture's as the recipe normalises them",
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
    assert!(rendered.contains("| ext/ctype | pass |"), "{rendered}");
    assert!(rendered.contains("| ext/hash | fail |"), "{rendered}");
}

/// `--ext <name>` is the on-request form (harness §7): one extension, one
/// reading, the engine's side rendered through the linked adapter, which
/// is what a pass means with no directory of renderings handed in.
#[test]
fn ext_names_one_extension_rendered_through_the_linked_adapter_and_writes_one_reading() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new("one");
    let ask = one("ctype");
    assert!(ask.renderings.is_none(), "the linked engine renders");
    let reading = scratch.one(&ask);
    assert_eq!(reading.scope, "ext/ctype");
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
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
/// for the engine's is a pass, and the one a byte apart is not, the
/// reading naming the line.
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
        reading.message.contains("at line 4, 2 lines in all")
            && reading
                .message
                .contains("-    Function [ <internal:ctype> function ctype_alnum ] {")
            && reading
                .message
                .contains("+    Function [ <internal:ctype> function ctype_alnun ] {"),
        "{}",
        reading.message
    );
}

/// The load number (#317): the rendering whose `#10` stands as `#7` is a
/// pass through `run` under the pin's `extension_number_token` row, both
/// streams renumbered before the verdict, and the oracle's bytes are kept
/// as they were compared; the same bytes under a pin without the row are
/// a fail naming the number on both sides.
#[test]
fn the_renumbered_fixture_through_run_is_a_pass_under_the_row_and_not_without_it() {
    if oracle().is_none() {
        return;
    }
    let mut scratch = Scratch::new("renumbered");
    let token = scratch
        .manifest
        .pin
        .recipe
        .normalise
        .extension_number_token
        .clone()
        .expect("the pin carries the row");
    let ask = Ask {
        renderings: Some(scratch.renderings("renumbered.re")),
        ..one("ctype")
    };
    let reading = scratch.one(&ask);
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    let kept = fs::read(crosscheck::kept(
        &scratch.store,
        &scratch.manifest.pin.oracle.md5,
        "ctype",
    ))
    .unwrap();
    assert!(
        kept.starts_with(format!("Extension [ <persistent> extension #{token} ctype").as_bytes()),
        "the kept bytes are the compared ones: {}",
        String::from_utf8_lossy(&kept[..60])
    );

    scratch.manifest.pin.recipe.normalise.extension_number_token = None;
    let reading = scratch.one(&ask);
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert!(
        reading
            .message
            .contains("-Extension [ <persistent> extension #10 ctype")
            && reading
                .message
                .contains("+Extension [ <persistent> extension #7 ctype"),
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
