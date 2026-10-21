//! `xtask port-map` end to end (third-engine-design.md §4.4; harness §4.2,
//! §7): the committed census joined to `api.lock`, one line per row, one
//! reading at the crate the lock lives with, the negative control that
//! names an `api` item the lock does not carry, and the two ways a tree
//! is read — regenerated, or skipped and said to be.

use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::port_map;
use xtask::reading::{Number, Status, Store};
use xtask::scratch::Tree;

mod common {
    pub mod repo;
}
use common::repo::Repo;

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

fn fixture(name: &str) -> PathBuf {
    xtask::package_dir().join("fixtures/port-map").join(name)
}

/// A scratch workspace with the tracked manifest, pin, census, lock and
/// the census command, committed so a reading can be stamped.
fn scratch(name: &str) -> Tree {
    let dir = Tree::new(&format!("port-map-{name}")).unwrap();
    for sub in ["data", "docs", "scripts", "crates/php_ext/src"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    for file in [
        "seams.toml",
        "oracle.toml",
        "docs/divergence.toml",
        port_map::SEED,
        port_map::CENSUS,
        "crates/php_ext/Cargo.toml",
        "crates/php_ext/api.lock",
        "crates/php_ext/src/lib.rs",
    ] {
        std::fs::copy(workspace().join(file), dir.join(file)).unwrap();
    }
    Repo::init(&dir).commit("scratch");
    dir
}

/// The committed census's rows of one status, read off its `status`
/// lines, so a row a commit marks gone moves the counts the tests expect
/// and not the tests (#2504).
fn marked(status: &str) -> i64 {
    let seed = std::fs::read_to_string(workspace().join(port_map::SEED)).unwrap();
    let line = format!("status = \"{status}\"");
    let n = seed.lines().filter(|l| l.trim() == line).count();
    i64::try_from(n).unwrap()
}

/// Every row of the committed census.
fn census_rows() -> i64 {
    let rows = marked("same") + marked("renamed") + marked("gone");
    assert!(rows > 600, "the census has its rows: {rows}");
    rows
}

fn xtask(root: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(xtask::scratch::xtask(env!("CARGO_BIN_EXE_xtask")))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

fn last(root: &Path) -> xtask::reading::Reading {
    Store::of_workspace(root)
        .read(port_map::INSTRUMENT, "php_ext")
        .unwrap()
        .expect("the instrument wrote its reading")
}

#[test]
fn the_run_prints_a_line_per_row_with_its_status_and_its_api_item() {
    let root = scratch("rows");
    let (ok, out, err) = xtask(&root, ["port-map"].as_slice());
    assert!(ok, "{err}{out}");
    let rows: Vec<&str> = out
        .lines()
        .filter(|l| l.starts_with("same ") || l.starts_with("renamed ") || l.starts_with("gone "))
        .collect();
    assert_eq!(
        i64::try_from(rows.len()).unwrap(),
        census_rows(),
        "one line per census row"
    );
    assert!(
        rows.iter().all(|l| l.contains("unresolved")),
        "no extension is ported, so no row is resolved"
    );
    assert!(
        out.contains("same     unresolved  mahout_value::{PhpArray,PhpString}::len → values::PhpArray::len  748 sites, 18 crates"),
        "{}",
        &out[..400.min(out.len())]
    );
    assert!(
        out.contains(
            "gone     unresolved  RequestCtx::note_alloc → nothing; the allocator accounts"
        ),
        "a gone row prints the rule that replaces the need"
    );
}

#[test]
fn the_reading_is_the_unresolved_rows_at_the_crate_the_lock_lives_with() {
    let root = scratch("reading");
    let (ok, out, err) = xtask(&root, ["port-map"].as_slice());
    assert!(ok, "{err}");
    let reading = last(&root);
    assert_eq!(reading.status, Status::Pass, "{reading:?}");
    assert_eq!(reading.instrument, port_map::INSTRUMENT);
    assert_eq!(reading.scope, "php_ext");
    let n = |status: &str| Number::from(marked(status));
    let rows = Number::from(census_rows());
    assert_eq!(reading.numbers["rows"], rows);
    assert_eq!(reading.numbers["same"], n("same"));
    assert_eq!(reading.numbers["renamed"], n("renamed"));
    assert_eq!(reading.numbers["gone"], n("gone"));
    assert_eq!(reading.numbers["unmatched"], 0.into());
    assert_eq!(reading.numbers["unresolved"], rows);
    assert!(!reading.commit.is_empty(), "stamped: {reading:?}");
    assert!(
        reading.message.contains(port_map::SEED)
            && reading.message.contains("crates/php_ext/api.lock"),
        "the reading says which census it read and what it joined to: {}",
        reading.message
    );
    assert!(out.contains(&reading.message), "{out}");
}

#[test]
fn the_fixture_fails_the_instrument_as_the_run_reads_it_and_the_exit_says_so() {
    let root = scratch("fixture");
    std::fs::copy(
        fixture("unmatched/port-map.toml"),
        root.join(port_map::SEED),
    )
    .unwrap();
    std::fs::copy(
        fixture("unmatched/api.lock"),
        root.join("crates/php_ext/api.lock"),
    )
    .unwrap();
    let (ok, out, err) = xtask(&root, ["port-map"].as_slice());
    assert!(!ok, "the unmatched row is a failure: {out}");
    assert!(err.contains("1 of 1 readings fail"), "{err}");
    assert_eq!(last(&root).status, Status::Fail);
}

/// Harness §4.2: the regeneration is the instrument's own flag, and a tree
/// it cannot read is a skip that says so rather than a pass over a census
/// it did not regenerate.
#[test]
fn a_tree_that_is_no_mahout_tree_is_a_skip_and_not_a_pass() {
    let root = scratch("absent");
    let absent = root.join("no-such-tree");
    let (ok, out, err) = xtask(
        &root,
        ["port-map", "--tree", absent.to_str().unwrap()].as_slice(),
    );
    assert!(ok, "a skip owes no failing exit: {err}");
    let reading = last(&root);
    assert_eq!(reading.status, Status::Skip, "{reading:?}");
    assert!(
        reading.message.contains("no crates/") && reading.message.contains(port_map::SEED),
        "the reading says the tree was unread and the committed census was not read as one: {}",
        reading.message
    );
    assert!(
        !out.contains("mahout_value::{PhpArray,PhpString}::len"),
        "nothing was joined, so no row is printed"
    );
}

#[test]
fn a_tree_the_census_refuses_is_a_skip_that_names_what_it_said() {
    let root = scratch("refused");
    let tree = root.join("tree");
    std::fs::create_dir_all(tree.join("src")).unwrap();
    let (ok, _, err) = xtask(
        &root,
        ["port-map", "--tree", tree.to_str().unwrap()].as_slice(),
    );
    assert!(ok, "{err}");
    assert_eq!(last(&root).status, Status::Skip);

    std::fs::create_dir_all(tree.join("crates")).unwrap();
    let (ok, _, err) = xtask(
        &root,
        ["port-map", "--tree", tree.to_str().unwrap()].as_slice(),
    );
    assert!(ok, "{err}");
    let reading = last(&root);
    assert_eq!(
        reading.status,
        Status::Skip,
        "a tree with no ext crate is read by nothing: {reading:?}"
    );
    assert!(
        reading.message.contains(port_map::CENSUS),
        "the reading names what refused: {}",
        reading.message
    );
}

#[test]
fn status_renders_the_reading_as_one_row_of_its_own_section() {
    let root = scratch("status");
    let (ok, _, err) = xtask(&root, ["port-map"].as_slice());
    assert!(ok, "{err}");
    let (ok, md, err) = xtask(&root, ["status", "--section", "port-map"].as_slice());
    assert!(ok, "{err}");
    assert!(
        md.contains("php_ext") && md.contains(&format!("unresolved={}", census_rows())),
        "{md}"
    );
}
