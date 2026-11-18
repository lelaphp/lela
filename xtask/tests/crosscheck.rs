//! `xtask crosscheck`, the rendering diff (design §7.1; harness §7, ledger
//! 41): each extension of the bundle's default set rendered by the pinned
//! oracle's `php --re <name>` under the recipe, kept in the store, and
//! held against the engine's rendering, one reading per extension per
//! column, `--re` and `--ri`. Every
//! test here goes through the instrument's `run` and reads what it wrote;
//! the verdict on two renderings and the negative control are held
//! beside the lane, in its own tests.
//!
//! The tests that spawn the oracle say so on stderr and return when the
//! pinned binary is not on this machine, unless the machine promised it;
//! the rest run anywhere.
#![cfg(tracked)]

use std::fs;
use std::path::{Path, PathBuf};

use xtask::carried::Carried;
use xtask::cli::{Format, Section};
use xtask::crosscheck::{self, Ask, Column};
use xtask::manifest::Manifest;
use xtask::oracle::{self, Promise};
use xtask::reading::{Number, Reading, Status, Store};
use xtask::scratch::Tree;

mod common {
    pub mod manifest;
    pub mod registry;
    pub mod repo;
    pub mod run;
}
use common::manifest::{of_rows, pin};
use common::registry::registry;
use common::repo::scratch_workspace;
use common::run::xtask;

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

/// The manifest a scratch judges by: rows for the two extensions these
/// tests render, under the tracked pin. The bundle's default set as the
/// tracked rows state it is held in [`tracked`].
fn manifest() -> Manifest {
    of_rows([
        xtask::manifest::Crate::named("ext/ctype"),
        xtask::manifest::Crate::named("ext/json"),
    ])
}

/// The tracked pin's binary, when it is on this machine; a failure where
/// this machine promised the binary.
fn oracle() -> Option<PathBuf> {
    let pin = pin();
    Promise::Oracle
        .or_skip(pin.oracle.file().map(Path::to_path_buf))
        .unwrap()
}

fn fixtures() -> PathBuf {
    xtask::package_dir().join("fixtures/crosscheck")
}

/// A workspace of this test's own: a git repository holding the tracked
/// manifest and pin, and a store of its own for the readings and the
/// oracle's bytes; it judges by [`manifest`].
struct Scratch {
    root: Tree,
    store: Store,
    manifest: Manifest,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let root = Tree::new(&format!("crosscheck-{name}")).unwrap();
        scratch_workspace(workspace(), &root);
        let store = Store::at(root.join("target/harness"));
        Scratch {
            root,
            store,
            manifest: manifest(),
        }
    }

    fn run(&self, ask: &Ask) -> Vec<Reading> {
        let readings =
            crosscheck::run(&self.root, &self.manifest, &registry(""), &self.store, ask).unwrap();
        self.store.record(&readings).unwrap();
        readings
    }

    /// One extension's two readings, `--re` then `--ri`.
    fn two(&self, ask: &Ask) -> (Reading, Reading) {
        let readings = self.run(ask);
        assert_eq!(readings.len(), 2, "{readings:#?}");
        let mut readings = readings.into_iter();
        (readings.next().unwrap(), readings.next().unwrap())
    }

    /// One extension's `--re` reading, the `--ri` one beside it.
    fn one(&self, ask: &Ask) -> Reading {
        self.two(ask).0
    }

    /// A directory of renderings standing for the engine's: `ctype.re`
    /// holding the `.re` fixture named and `ctype.ri` the `.ri` one.
    fn renderings(&self, re: &str, ri: &str) -> PathBuf {
        let dir = self.root.join("renderings");
        fs::create_dir_all(&dir).unwrap();
        fs::copy(fixtures().join(re), dir.join("ctype.re")).unwrap();
        fs::copy(fixtures().join(ri), dir.join("ctype.ri")).unwrap();
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

/// The floor: the scopes whose rendering is the oracle's and may not stop
/// being, one entry per column (harness-design.md §7, ledger 49).
/// Under `--re`: ctype, json, reflection, whose twenty-six
/// classes render whole from the commit that declared them (d8bc96d), and
/// tokenizer, whose constants and signatures reached php's order in
/// 2f23e89. Under `--ri`: those four and `random` beside them,
/// whose whole table is the `Version` line every extension's header
/// carries (a3c01d0 wrote tokenizer's).
///
/// It is a floor and not the set of what passes. An extension outside it
/// reads whatever the port has reached, held to the shape of its message
/// and not to a status, so a commit to `ext/<name>` that carries a
/// rendering to the oracle's bytes moves this lane's readings and leaves
/// the harness's own suite green. Raising it is one line beside such a
/// commit and nothing forces it; a scope in it that stops passing is the
/// regression this test is here for.
///
/// It rises onto one of the six extensions whose columns differ between
/// the keg and a Linux build (`ext/standard`, `ext/pcre`, `ext/pdo`,
/// `ext/pdo_sqlite`, `ext/dom`, `ext/iconv`) only under the ruling of
/// 2026-09-28 (harness-design.md §7, revision 103): when the engine renders
/// the pinned keg's bytes, with no library version or driver list
/// normalised.
const FLOOR: [&str; 9] = [
    "ext/ctype",
    "ext/ctype/ri",
    "ext/json",
    "ext/json/ri",
    "ext/random/ri",
    "ext/reflection",
    "ext/reflection/ri",
    "ext/tokenizer",
    "ext/tokenizer/ri",
];

/// Every reading of `FLOOR` that is not a pass, named: what turns the
/// harness's own suite red is a rendering that went backwards, and an
/// extension the port has not reached yet is not one. A scope
/// absent from `readings` is not read here, so the caller that runs the
/// whole default set holds the floor's presence beside this.
fn below_the_floor(readings: &[Reading]) -> Result<(), String> {
    let short: Vec<&str> = readings
        .iter()
        .filter(|r| FLOOR.contains(&r.scope.as_str()) && r.status != Status::Pass)
        .map(|r| r.scope.as_str())
        .collect();
    if short.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "below the floor, the rendering no longer the oracle's at: {}",
            short.join(", ")
        ))
    }
}

/// Design §11 days 1–2, the two halves met and the second column
/// beside them: every extension of the default set is rendered by
/// the oracle and by the linked engine under `--re` and under `--ri`, two
/// readings each, the `--ri` one scoped `ext/<name>/ri`. What each row
/// reads is the port's reach and is read off the run rather than pinned
/// here: a pass says the engine's rendering is the oracle's, a
/// fail names the engine's rendering as the cause, the first differing
/// line or the engine's own words for rendering nothing, and `FLOOR` is
/// what may not go backwards. The oracle's bytes are kept for each
/// column, and the section renders every row with the status its reading
/// carries.
/// Whether a reading rendered anything: a row laid before its crate is a
/// skip that says it is not yet created.
fn rendered(reading: &Reading) -> bool {
    !reading.message.ends_with(": not yet created")
}

/// The floor is a sensor and not a comment (harness-design.md §7,
/// ledger 49). `ext/ctype`'s `--re` rendering carried one byte off the
/// oracle's is below the floor and the failure names that scope alone,
/// its `--ri` column beside it untouched; an extension outside the floor
/// failing in both columns is not below it. So what leaves `cargo test -p
/// xtask` red is a rendering that went backwards, and the port advancing
/// somewhere else is not.
#[test]
fn a_scope_of_the_floor_that_stops_passing_is_below_it_and_one_outside_it_is_not() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new("floor");
    let apart = Ask {
        renderings: Some(scratch.renderings("engine.re", "oracle.ri")),
        ..one("ctype")
    };
    let (re, ri) = scratch.two(&apart);
    assert_eq!(re.status, Status::Fail, "{re:#?}");
    assert_eq!(ri.status, Status::Pass, "{ri:#?}");
    let why = below_the_floor(&[re, ri]).unwrap_err();
    assert!(
        why.contains("ext/ctype") && !why.contains("ext/ctype/ri"),
        "{why}"
    );

    let outside = scratch.run(&one("nosuch"));
    assert!(
        outside.iter().all(|r| r.status == Status::Fail),
        "{outside:#?}"
    );
    assert_eq!(below_the_floor(&outside), Ok(()), "{outside:#?}");
}

/// `--ext <name>` is the on-request form (harness §7): one extension, two
/// readings, the engine's side rendered through the linked adapter, which
/// is what a pass means with no directory of renderings handed in. The
/// `--ri` reading is in the store under `ext/<name>/ri`, stamped over the
/// crate's tree as the `--re` one is, so the two go stale together.
#[test]
fn ext_names_one_extension_rendered_through_the_linked_adapter_and_writes_two_readings() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new("one");
    let ask = one("ctype");
    assert!(ask.renderings.is_none(), "the linked engine renders");
    let (re, ri) = scratch.two(&ask);
    assert_eq!(re.scope, "ext/ctype");
    assert_eq!(re.status, Status::Pass, "{re:#?}");
    assert!(re.message.contains("php -n --re ctype"), "{}", re.message);
    assert_eq!(ri.scope, "ext/ctype/ri");
    assert_eq!(ri.status, Status::Pass, "{ri:#?}");
    assert!(ri.message.contains("php -n --ri ctype"), "{}", ri.message);
    assert_eq!(count(&ri, "bytes"), 33, "{ri:#?}");
    assert_eq!(
        re.key(),
        ri.key(),
        "both columns are stamped over ext/ctype"
    );
    for scope in ["ext/ctype", "ext/ctype/ri"] {
        assert!(
            scratch.store.read("crosscheck", scope).unwrap().is_some(),
            "the reading for {scope} is in the store"
        );
    }
}

/// An extension the oracle does not have is a fail carrying php's own
/// words, never a panic.
#[test]
fn an_extension_the_oracle_does_not_have_fails_with_phps_own_words() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new("nosuch");
    let (re, ri) = scratch.two(&one("nosuch"));
    assert_eq!(re.scope, "ext/nosuch");
    assert_eq!(re.status, Status::Fail, "{re:#?}");
    assert!(
        re.message.contains("Extension \"nosuch\" does not exist"),
        "{}",
        re.message
    );
    assert_eq!(ri.scope, "ext/nosuch/ri");
    assert_eq!(ri.status, Status::Fail, "{ri:#?}");
    assert!(
        ri.message.contains("Extension 'nosuch' not present."),
        "{}",
        ri.message
    );
    for column in Column::ALL {
        assert!(
            !crosscheck::kept(
                &scratch.store,
                &scratch.manifest.pin().unwrap().oracle.md5,
                "nosuch",
                column
            )
            .exists(),
            "nothing is kept for an extension that rendered nothing"
        );
    }
}

/// The negative control (harness §14 item 4): php's own rendering standing
/// for the engine's is a pass in each column, and the one a byte apart is
/// not, the reading naming the line; a byte in one column fails that
/// column alone.
#[test]
fn the_fixture_through_run_is_a_pass_and_the_one_byte_apart_is_not() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new("fixture");
    let same = Ask {
        renderings: Some(scratch.renderings("oracle.re", "oracle.ri")),
        ..one("ctype")
    };
    let (re, ri) = scratch.two(&same);
    assert_eq!(re.status, Status::Pass, "{re:#?}");
    assert_eq!(ri.status, Status::Pass, "{ri:#?}");

    let apart = Ask {
        renderings: Some(scratch.renderings("engine.re", "oracle.ri")),
        ..one("ctype")
    };
    let (re, ri) = scratch.two(&apart);
    assert_eq!(re.status, Status::Fail, "{re:#?}");
    assert_eq!(
        ri.status,
        Status::Pass,
        "a byte in --re is --re's alone: {ri:#?}"
    );
    assert!(
        re.message.contains("at line 4, 2 lines in all")
            && re
                .message
                .contains("-    Function [ <internal:ctype> function ctype_alnum ] {")
            && re
                .message
                .contains("+    Function [ <internal:ctype> function ctype_alnun ] {"),
        "{}",
        re.message
    );

    let apart = Ask {
        renderings: Some(scratch.renderings("oracle.re", "engine.ri")),
        ..one("ctype")
    };
    let (re, ri) = scratch.two(&apart);
    assert_eq!(
        re.status,
        Status::Pass,
        "a byte in --ri is --ri's alone: {re:#?}"
    );
    assert_eq!(ri.status, Status::Fail, "{ri:#?}");
    assert!(
        ri.message.contains("at line 3, 2 lines in all")
            && ri.message.contains("-ctype functions => enabled")
            && ri.message.contains("+ctype functions => Enabled"),
        "{}",
        ri.message
    );
}

/// The load number: the rendering whose `#10` stands as `#7` is a
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
        .pin()
        .unwrap()
        .recipe
        .normalise
        .extension_number_token
        .clone()
        .expect("the pin carries the row");
    let ask = Ask {
        renderings: Some(scratch.renderings("renumbered.re", "oracle.ri")),
        ..one("ctype")
    };
    let reading = scratch.one(&ask);
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    let kept = fs::read(crosscheck::kept(
        &scratch.store,
        &scratch.manifest.pin().unwrap().oracle.md5,
        "ctype",
        Column::Re,
    ))
    .unwrap();
    assert!(
        kept.starts_with(format!("Extension [ <persistent> extension #{token} ctype").as_bytes()),
        "the kept bytes are the compared ones: {}",
        String::from_utf8_lossy(&kept[..60])
    );

    scratch
        .manifest
        .pin
        .as_mut()
        .unwrap()
        .recipe
        .normalise
        .extension_number_token = None;
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
    scratch.manifest.pin.as_mut().unwrap().oracle.path = scratch.root.join("nowhere/php");
    let (re, ri) = scratch.two(&one("ctype"));
    for reading in [re, ri] {
        assert_eq!(reading.status, Status::Skip, "{reading:#?}");
        assert!(
            reading.message.contains("nowhere/php"),
            "{}",
            reading.message
        );
    }
}

/// `--staged`, pre-commit's: a commit that stages only a
/// harness file, which no extension's rendering reads, renders nothing
/// and writes no reading, where the lane without it renders every one.
#[test]
fn staged_with_no_file_a_rendering_reads_renders_nothing() {
    let root = Tree::new("crosscheck-staged").unwrap();
    scratch_workspace(workspace(), &root);
    fs::create_dir_all(root.join("xtask/src")).unwrap();
    fs::write(root.join("xtask/src/lib.rs"), "// moved\n").unwrap();
    let add = std::process::Command::new("git")
        .args(["add", "--", "xtask/src/lib.rs"])
        .current_dir(&root)
        .status()
        .unwrap();
    assert!(add.success());
    let stdout = xtask(&root, &["crosscheck", "--staged"]).ok();
    assert!(
        stdout.contains("no extension the binary carries renders again for what is staged"),
        "{stdout}"
    );
    let store = Store::of_workspace(&root);
    assert!(store.read("crosscheck", "ext/json").unwrap().is_none());
}

/// A `php-linked` that dies on a signal ends one extension's rendering,
/// not the run (design §8): the extension it died serving reads
/// `fail` in both columns naming the signal, and the next is served by a
/// binary started again and renders. The binary is a stand-in that
/// speaks `--serve`'s protocol, a request directory per line answered
/// `served`, killing itself on ctype's program and answering json's with
/// the oracle's own rendering.
#[test]
fn a_php_linked_that_dies_on_a_signal_fails_that_extension_and_the_next_still_renders() {
    let Some(php) = oracle() else {
        return;
    };
    let scratch = Scratch::new("signal");
    let server = scratch.root.join("php-linked");
    fs::write(
        &server,
        format!(
            "#!/bin/sh\nwhile read dir; do\n  case \"$(cat \"$dir/source\")\" in\n    *\"'ctype'\"*) kill -SEGV $$ ;;\n    *'->info()'*) flag=--ri ;;\n    *) flag=--re ;;\n  esac\n  '{}' -n $flag json > \"$dir/stdout\"\n  printf 0 > \"$dir/exit\"\n  echo served\ndone\n",
            php.display()
        ),
    )
    .unwrap();
    xtask::scratch::executable(&server).unwrap();
    let readings = scratch.run(&Ask {
        ext: vec!["ctype".to_owned(), "json".to_owned()],
        linked: Some(server),
        ..Ask::default()
    });
    assert_eq!(readings.len(), 4, "{readings:#?}");
    for reading in &readings[..2] {
        assert_eq!(reading.status, Status::Fail, "{reading:#?}");
        assert!(
            reading.message.contains("the engine rendered nothing")
                && reading.message.contains("died on signal 11"),
            "{}",
            reading.message
        );
    }
    for reading in &readings[2..] {
        assert!(reading.scope.starts_with("ext/json"), "{reading:#?}");
        assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    }
}

/// The tests whose subject is the tracked workspace itself: each reads the
/// `seams.toml`, `oracle.toml`, registry or sources standing in the
/// workspace, so a person's edit there is what may turn one red.
/// Every other test here judges by a manifest it builds, or by a
/// scratch's byte-for-byte copy of the tracked one where the binary's
/// hook or `permit` refuses any other (`built.rs`).
mod tracked {
    use super::*;

    #[test]
    fn every_default_extension_reads_the_ports_reach_in_both_columns_and_the_floor_holds() {
        if oracle().is_none() {
            return;
        }
        let mut scratch = Scratch::new("default");
        scratch.manifest = Manifest::load(workspace()).unwrap();
        let readings = scratch.run(&Ask::default());
        let carried = Carried::binary(&scratch.root).unwrap();
        let default: Vec<&str> = scratch
            .manifest
            .crates
            .iter()
            .filter(|c| c.name.starts_with("ext/") && carried.carries(c))
            .map(|c| c.name.as_str())
            .collect();
        assert!(default.contains(&"ext/ctype"), "{default:?}");
        assert!(default.contains(&"ext/json"), "{default:?}");
        assert!(default.contains(&"ext/hash"), "{default:?}");
        let scopes: Vec<&str> = readings.iter().map(|r| r.scope.as_str()).collect();
        let expected: Vec<String> = default
            .iter()
            .flat_map(|name| [(*name).to_owned(), format!("{name}/ri")])
            .collect();
        assert_eq!(scopes, expected);
        for scope in FLOOR {
            assert!(scopes.contains(&scope), "the floor's {scope} is run");
        }
        for reading in readings.iter().filter(|r| rendered(r)) {
            assert_eq!(reading.instrument, "crosscheck");
            let flag = if reading.scope.ends_with("/ri") {
                "--ri"
            } else {
                "--re"
            };
            assert!(reading.message.contains(flag), "{}", reading.message);
            assert!(count(reading, "bytes") > 0, "{reading:#?}");
            match reading.status {
                Status::Pass => assert!(
                    reading
                        .message
                        .contains("the engine's rendering is the oracle's"),
                    "{}",
                    reading.message
                ),
                Status::Fail => assert!(
                    reading
                        .message
                        .contains("the engine's rendering differs from the oracle's at line")
                        || reading.message.contains("the engine rendered nothing"),
                    "{}",
                    reading.message
                ),
                Status::Skip => panic!("a rendering is a pass or a fail: {reading:#?}"),
            }
        }
        if let Err(why) = below_the_floor(&readings) {
            panic!("{why}");
        }
        let kept_ri = crosscheck::kept(
            &scratch.store,
            &scratch.manifest.pin().unwrap().oracle.md5,
            "ctype",
            Column::Ri,
        );
        assert_eq!(
            fs::read(&kept_ri).unwrap(),
            fs::read(fixtures().join("oracle.ri")).unwrap(),
            "the oracle's --ri bytes at {} are the fixture's",
            kept_ri.display()
        );
        let kept = crosscheck::kept(
            &scratch.store,
            &scratch.manifest.pin().unwrap().oracle.md5,
            "ctype",
            Column::Re,
        );
        // The fixture is php's own bytes; what is kept is what a run compares,
        // the fixture under the recipe's normalisation, its load number the
        // pin's token.
        assert_eq!(
            fs::read(&kept).unwrap(),
            oracle::normalise(
                &scratch.manifest.pin().unwrap().recipe.normalise,
                fs::read(fixtures().join("oracle.re")).unwrap()
            ),
            "the oracle's bytes at {} are the fixture's as the recipe normalises them",
            kept.display()
        );
        let rendered = xtask::status::render(
            &scratch.manifest,
            &registry(""),
            xtask::status::Against::Checkout(&scratch.store),
            &scratch.root,
            Section::Crosscheck,
            Format::Md,
            None,
            false,
        )
        .unwrap();
        // Every row the run wrote, with the status its reading carries: the
        // section renders what the lane read and never a set pinned here.
        for reading in &readings {
            let row = format!("| {} | {} |", reading.scope, reading.status.as_str());
            assert!(rendered.contains(&row), "{row} in {rendered}");
        }
    }
}
