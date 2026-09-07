//! `xtask api-check` over the fixtures of `xtask/fixtures/api-check`, judged
//! by the tracked manifest: locks, edges and bundle order (ticket #4). One
//! fixture per rule the instrument evaluates must fail; `clean` must pass.

use std::path::{Path, PathBuf};

use xtask::api_check;
use xtask::manifest::Manifest;
use xtask::reading::{Reading, Status};

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
    api_check::check(&manifest, &fixture(name), false).unwrap()
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
        assert_eq!(reading.tree.len(), 40, "{}", reading.scope);
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

#[test]
fn api_lock_over_the_ceiling_fails() {
    let readings = readings("ceiling");
    let api = of(&readings, "api");
    assert_eq!(api.status, Status::Fail, "{}", api.message);
    assert_eq!(api.numbers["items"], 401.into());
    assert!(api.message.contains("401"), "{}", api.message);
    assert!(api.message.contains("400"), "{}", api.message);
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

/// The tracked workspace: every seam whose producer does not exist yet is a
/// skip, and the harness itself is within its edges.
#[test]
fn the_workspace_today_has_no_failing_reading() {
    let manifest = Manifest::load(workspace()).unwrap();
    let readings = api_check::check(&manifest, workspace(), false).unwrap();
    let failed: Vec<String> = failures(&readings)
        .iter()
        .map(|r| format!("{}: {}", r.scope, r.message))
        .collect();
    assert!(failed.is_empty(), "{}", failed.join("\n"));
    assert_eq!(of(&readings, "xtask").status, Status::Pass);
    assert_eq!(
        readings.iter().filter(|r| r.status == Status::Skip).count(),
        manifest.seams.len(),
        "one skip per seam: {readings:#?}"
    );
}
