//! `xtask api-check` over the fixtures of `xtask/fixtures/api-check`, judged
//! by the tracked manifest: locks, edges and bundle order (ticket #4). One
//! fixture per rule the instrument evaluates must fail; `clean` must pass.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use xtask::api_check;
use xtask::manifest::{Manifest, OneOrMany, Rule, Seam};
use xtask::reading::{Reading, Status};
use xtask::scratch::Tree;

mod common;
use common::scratch_workspace;

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/api-check")
        .join(name)
}

fn readings(name: &str) -> Vec<Reading> {
    let manifest = Manifest::load(workspace()).unwrap();
    api_check::hold(&manifest, &fixture(name), false).unwrap()
}

fn failures(readings: &[Reading]) -> Vec<&Reading> {
    readings
        .iter()
        .filter(|r| r.status == Status::Fail)
        .collect()
}

fn of<'a>(readings: &'a [Reading], scope: &str) -> &'a Reading {
    readings
        .iter()
        .find(|r| r.scope == scope)
        .unwrap_or_else(|| panic!("no reading of scope {scope}"))
}

#[test]
fn the_clean_fixture_passes_every_rule() {
    let readings = readings("clean");
    let failed: Vec<String> = failures(&readings)
        .iter()
        .map(|r| format!("{}: {}", r.scope, r.message))
        .collect();
    assert!(failed.is_empty(), "{}", failed.join("\n"));

    let heap = of(&readings, "heap");
    assert_eq!(heap.status, Status::Pass, "{}", heap.message);
    assert_eq!(heap.numbers["items"], 9.into());
    let api = of(&readings, "api");
    assert_eq!(api.status, Status::Pass, "{}", api.message);
    assert_eq!(api.numbers["items"], 8.into(), "re-exports are followed");
    assert_eq!(api.adr, "0002, 0004, 0005");
    assert_eq!(api.rule, "api.lock-line");
    assert_eq!(of(&readings, "ext/random").rule, "ext.pub-beyond-extension");
    assert!(
        of(&readings, "ext/random").message.contains("Handle"),
        "the pub surface beyond extension() is printed"
    );
    assert_eq!(of(&readings, "sapi").status, Status::Pass);
    assert_eq!(
        of(&readings, "unit").status,
        Status::Skip,
        "no php_unit here"
    );
    assert_eq!(of(&readings, "php_bundle").status, Status::Pass);
    assert_eq!(of(&readings, "ext/standard").status, Status::Pass);
    for reading in &readings {
        assert_eq!(reading.instrument, "api-check");
        assert_eq!(reading.tree().len(), 40, "{}", reading.scope);
    }
}

#[test]
fn a_public_item_without_a_lock_line_fails_naming_the_lock_file() {
    let readings = readings("lock-line");
    let heap = of(&readings, "heap");
    assert_eq!(heap.status, Status::Fail);
    assert!(
        heap.message.contains("crates/php_heap/heap.lock"),
        "{}",
        heap.message
    );
    assert!(heap.message.contains("alloc"), "{}", heap.message);
    assert_eq!(heap.adr, "0003");
    assert_eq!(failures(&readings).len(), 1);
}

/// One order for a lock's lines (#403): `--write` renders them ascending
/// and a check that read the set alone accepted a file it would rewrite,
/// which is how `crates/php_heap/heap.lock` carried `Arr::live_from`
/// before `Arr::len` into every commit whose write was taken for another
/// seam. The `lock-order` fixture has a line for every item and the two
/// lines the wrong way round, so nothing is missing and nothing is stale;
/// what fails is the order, and the reading names both lines. The
/// positive control is the `clean` fixture's own `heap.lock`, held by
/// `the_clean_fixture_passes_every_rule`.
#[test]
fn a_lock_in_an_order_write_would_rewrite_fails_naming_the_line_and_its_place() {
    let readings = readings("lock-order");
    let heap = of(&readings, "heap");
    assert_eq!(heap.status, Status::Fail, "{}", heap.message);
    assert!(
        heap.message
            .contains("crates/php_heap/heap.lock is not in the order")
            && heap.message.contains("alloc stands where Arena is written"),
        "{}",
        heap.message
    );
    assert!(
        !heap.message.contains("have no line") && !heap.message.contains("name no"),
        "every item has its line: {}",
        heap.message
    );
    assert_eq!(failures(&readings).len(), 1);
}

/// The `ceiling` fixture is one item past `[budget] api_items` whatever the
/// tracked manifest carries (#338): the fixture directory holds the
/// crate's `Cargo.toml`, and the items and their lock lines are laid down
/// here in a scratch workspace from the ceiling the manifest reads, so a
/// person moving the ceiling moves the fixture with it.
#[test]
fn api_lock_over_the_ceiling_fails() {
    let manifest = Manifest::load(workspace()).unwrap();
    let seam = manifest
        .seams
        .iter()
        .find(|s| s.name == "api")
        .expect("the manifest has the api seam");
    let ceiling = manifest
        .lock_ceiling(seam)
        .expect("the api seam has a ceiling")
        .items;
    let items = ceiling + 1;

    // A reading hashes its scope's tree through git, so the scratch tree
    // is a repository, the way every scratch workspace here is.
    let scratch = Tree::new("api-check-ceiling").unwrap();
    scratch_workspace(workspace(), &scratch).unwrap();
    let crate_dir = scratch.join("crates/php_ext");
    fs::create_dir_all(crate_dir.join("src")).unwrap();
    fs::copy(
        fixture("ceiling").join("crates/php_ext/Cargo.toml"),
        crate_dir.join("Cargo.toml"),
    )
    .unwrap();
    let names = (0..items)
        .map(|i| format!("ITEM_{i:04}"))
        .collect::<Vec<_>>();
    let mut lib = format!("//! {items} public items: one over the ceiling of {ceiling}.\n");
    let mut lock = format!("# {items} items, every one with a line; the ceiling is what fails.\n");
    for name in &names {
        writeln!(lib, "pub const {name}: u8 = 0;").unwrap();
        writeln!(lock, "{name}").unwrap();
    }
    fs::write(crate_dir.join("src/lib.rs"), lib).unwrap();
    fs::write(crate_dir.join("api.lock"), lock).unwrap();

    let readings = api_check::hold(&manifest, &scratch, false).unwrap();
    let api = of(&readings, "api");
    assert_eq!(api.status, Status::Fail, "{}", api.message);
    assert_eq!(api.numbers["items"], i64::try_from(items).unwrap().into());
    assert!(api.message.contains(&items.to_string()), "{}", api.message);
    assert!(
        api.message.contains(&ceiling.to_string()),
        "{}",
        api.message
    );
}

#[test]
fn a_crate_depending_outside_its_edges_fails_with_the_adr() {
    let readings = readings("edge");
    let json = of(&readings, "ext/json");
    assert_eq!(json.status, Status::Fail);
    assert!(json.message.contains("php_engine"), "{}", json.message);
    assert!(
        json.message
            .contains("ADR-0004: Extensions are one crate each under `ext/`"),
        "the ADR's text is printed: {}",
        json.message
    );
    assert_eq!(json.adr, "0004");
}

#[test]
fn a_use_path_across_a_denied_module_edge_fails() {
    let readings = readings("deny-edge");
    let engine = of(&readings, "php_engine");
    assert_eq!(engine.status, Status::Fail);
    assert!(engine.message.contains("executor"), "{}", engine.message);
    assert!(engine.message.contains("serve"), "{}", engine.message);
    assert!(
        engine.message.contains("src/executor.rs"),
        "the file is named: {}",
        engine.message
    );
}

#[test]
fn a_bundle_order_that_is_not_topological_fails() {
    let readings = readings("bundle");
    let bundle = of(&readings, "php_bundle");
    assert_eq!(bundle.status, Status::Fail);
    assert!(
        bundle.message.contains("ext/standard"),
        "{}",
        bundle.message
    );
    assert!(bundle.message.contains("ext/random"), "{}", bundle.message);
    assert!(bundle.adr.contains("0004"), "{}", bundle.adr);
    assert_eq!(failures(&readings).len(), 1, "{readings:#?}");
}

/// The tracked workspace: every seam whose lock has no home present yet,
/// that is generated from a file the harness renders no lock from, or
/// that is diffed per section, is a skip, and the harness itself is
/// within its edges.
#[test]
fn the_workspace_today_has_no_failing_reading() {
    let manifest = Manifest::load(workspace()).unwrap();
    let readings = api_check::hold(&manifest, workspace(), false).unwrap();
    let failed: Vec<String> = failures(&readings)
        .iter()
        .map(|r| format!("{}: {}", r.scope, r.message))
        .collect();
    assert!(failed.is_empty(), "{}", failed.join("\n"));
    assert_eq!(of(&readings, "xtask").status, Status::Pass);
    let unmeasured = manifest
        .seams
        .iter()
        .filter(|seam| {
            (seam.generated && seam.producer != "ops.toml")
                || !seam.sections.is_empty()
                || manifest
                    .lock_home(&seam.lock)
                    .and_then(|home| manifest.crate_named(&home))
                    .is_none_or(|home| !workspace().join(home.path()).join("Cargo.toml").is_file())
        })
        .count();
    assert_eq!(
        readings.iter().filter(|r| r.status == Status::Skip).count(),
        unmeasured,
        "one skip per seam without a home present or measured: {readings:#?}"
    );
}

/// A hand-edited line of `ops.lock`: the one lock rendered from a file
/// rather than from a crate's sources, held by the same instrument as
/// the other six (#95). The fixture's `ADD` line has lost its `throw`
/// effect, so the rendered line has no line in the lock and the line
/// names no opcode.
#[test]
fn a_hand_edited_line_of_ops_lock_fails() {
    let readings = readings("ops-lock");
    let ops = of(&readings, "ops");
    assert_eq!(ops.status, Status::Fail, "{}", ops.message);
    assert_eq!(ops.numbers["items"], 3.into());
    assert!(
        ops.message.contains("crates/php_unit/ops.lock"),
        "the lock is named: {}",
        ops.message
    );
    assert!(
        ops.message.contains("alloc,throw"),
        "the rendered line is named: {}",
        ops.message
    );
    assert!(
        ops.message.contains("name no opcode"),
        "the hand edit is named as stale: {}",
        ops.message
    );
    assert_eq!(failures(&readings).len(), 1, "{readings:#?}");
}

/// The `ops` seam's lock lives with `php_unit`, which produces no part of
/// it: the `[lock_home]` table names the home, since the seam's producer
/// is a file and no `[[crate]]` row carries the lock.
#[test]
fn the_ops_lock_is_at_home_with_php_unit() {
    let manifest = Manifest::load(workspace()).unwrap();
    assert_eq!(
        manifest.lock_home("ops.lock").as_deref(),
        Some("php_unit"),
        "the manifest names the lock's home"
    );
    let readings = api_check::hold(&manifest, workspace(), false).unwrap();
    let ops = of(&readings, "ops");
    assert_eq!(ops.status, Status::Pass, "{}", ops.message);
    assert_eq!(ops.numbers["items"], 209.into());
    assert!(
        ops.message.contains("crates/php_unit/ops.lock"),
        "{}",
        ops.message
    );
}

/// The rows a person adds for the harness's own lock (#55, harness §11:
/// the agent proposes, the person commits), on a manifest that does not
/// carry them yet: the `harness` seam over `xtask.lock`, the lock on the
/// `xtask` crate row, and the `xtask.lock-line` rule that notes a new
/// public item at the call. Once the tracked manifest carries them
/// this adds nothing, and the two tests below hold the tracked rows.
fn with_the_harness_rows(mut manifest: Manifest) -> Manifest {
    if !manifest.seams.iter().any(|s| s.lock == "xtask.lock") {
        let mut seam = Seam::named("harness", "xtask");
        "xtask.lock".clone_into(&mut seam.lock);
        seam.consumers = vec!["xtask".to_owned()];
        manifest.seams.push(seam);
    }
    let row = manifest
        .crates
        .iter_mut()
        .find(|c| c.name == "xtask")
        .expect("the manifest has the xtask row");
    if !row.lock.iter().any(|l| l == "xtask.lock") {
        row.lock = OneOrMany::One("xtask.lock".to_owned());
    }
    if manifest.rule("xtask.lock-line").is_none() {
        let mut rule = Rule::named("xtask.lock-line", "xtask/src/**");
        rule.predicate.deny = None;
        rule.predicate.on = Some("pub".to_owned());
        "A new pub item in xtask needs its xtask.lock line in the same commit."
            .clone_into(&mut rule.text);
        manifest.rules.push(rule);
    }
    manifest
}

/// The negative control of the harness's lock: a public item with no line
/// in `xtask/xtask.lock` fails the `harness` seam, the reading names the
/// lock and the item and carries the `on = "pub"` rule over `xtask/src`,
/// and a `pub(crate)` item is no public item.
#[test]
fn a_public_item_of_the_harness_without_its_lock_line_fails_naming_xtask_lock() {
    let manifest = with_the_harness_rows(Manifest::load(workspace()).unwrap());
    let readings = api_check::hold(&manifest, &fixture("harness-lock"), false).unwrap();
    let harness = of(&readings, "harness");
    assert_eq!(harness.status, Status::Fail, "{}", harness.message);
    assert!(
        harness.message.contains("xtask/xtask.lock"),
        "{}",
        harness.message
    );
    assert!(harness.message.contains("measure"), "{}", harness.message);
    assert!(
        !harness.message.contains("inside"),
        "pub(crate) is not public: {}",
        harness.message
    );
    assert_eq!(harness.rule, "xtask.lock-line");
    assert_eq!(harness.numbers["items"], 2.into());
    assert_eq!(failures(&readings).len(), 1, "{readings:#?}");
}

/// The committed `xtask/xtask.lock` is the harness's public items as they
/// stand: held here under the rows before a person commits them, so a new
/// public item without its line fails this test the way it will fail
/// `api-check` once the row is the manifest's, and `xtask api-check
/// --write` is what regenerates it then.
#[test]
fn the_harness_lock_is_current_over_the_tracked_workspace() {
    let manifest = with_the_harness_rows(Manifest::load(workspace()).unwrap());
    let readings = api_check::hold(&manifest, workspace(), false).unwrap();
    let harness = of(&readings, "harness");
    assert_eq!(
        harness.status,
        Status::Pass,
        "{}; a new public item of xtask needs its line in xtask/xtask.lock in the same commit",
        harness.message
    );
    assert!(
        harness.message.contains("xtask/xtask.lock"),
        "{}",
        harness.message
    );
    assert_eq!(harness.rule, "xtask.lock-line");
    assert_eq!(
        harness.tree(),
        of(&readings, "xtask").tree(),
        "the lock's reading is stamped over its home, the harness"
    );
    let failed: Vec<String> = failures(&readings)
        .iter()
        .map(|r| format!("{}: {}", r.scope, r.message))
        .collect();
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}
