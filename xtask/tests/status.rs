//! `xtask status`, the one renderer (harness-design.md §4.6): it reads the
//! manifest and the store and computes nothing else.

use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::reading::{Reading, Status, Store};

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

/// A scratch workspace: the tracked manifest, pin and registry, and no store.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("xtask-status-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    for file in ["seams.toml", "oracle.toml", "docs/divergence.toml"] {
        std::fs::copy(workspace().join(file), dir.join(file)).unwrap();
    }
    dir
}

fn status(root: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("--root")
        .arg(root)
        .arg("status")
        .args(args)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

#[test]
fn rules_prints_each_rule_with_its_adr_and_failure_text() {
    let root = scratch("rules");
    let (ok, md, err) = status(&root, &["--section", "rules"]);
    assert!(ok, "{err}");
    assert!(md.contains("ext.no-locks"), "{md}");
    assert!(md.contains("ADR-0005"), "{md}");
    assert!(
        md.contains("Extension state is request-scoped or immutable"),
        "{md}"
    );
    assert!(md.contains("harness.self"), "{md}");
    assert!(
        md.contains("no reading"),
        "a rule nothing has measured says so: {md}"
    );

    let (ok, json, _) = status(&root, &["--section", "rules", "--format", "json"]);
    assert!(ok);
    let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
    let row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "ext.no-locks")
        .unwrap();
    assert_eq!(row["adr"], serde_json::json!(["0005"]));
    assert_eq!(row["reading"], serde_json::Value::Null);
}

#[test]
fn every_section_reports_no_reading_when_the_store_is_gone() {
    let root = scratch("empty");
    for section in ["rules", "locks", "budget"] {
        let (ok, md, err) = status(&root, &["--section", section]);
        assert!(ok, "{section}: {err}");
        assert!(md.contains("no reading"), "{section}: {md}");
    }
    assert!(
        !root.join("target").exists(),
        "status ran nothing and wrote nothing"
    );
}

#[test]
fn locks_and_budget_render_the_readings_the_instruments_wrote() {
    let root = scratch("readings");
    let store = Store::of_workspace(&root);

    let mut api = Reading::new("api-check", "api");
    api.status = Status::Pass;
    api.numbers.insert("items".into(), 301.into());
    store.write(&api).unwrap();

    let mut heap = Reading::new("budget", "php_heap");
    heap.status = Status::Fail;
    heap.numbers.insert("lines".into(), 20001.into());
    heap.message = "php_heap: 20001 lines over the ceiling of 20000".into();
    store.write(&heap).unwrap();

    let (ok, md, _) = status(&root, &["--section", "locks"]);
    assert!(ok);
    assert!(
        md.contains("api.lock") && md.contains("301") && md.contains("400"),
        "{md}"
    );
    assert!(
        md.contains("heap.lock") && md.contains("no reading"),
        "{md}"
    );

    let (ok, md, _) = status(&root, &["--section", "budget"]);
    assert!(ok);
    assert!(
        md.contains("php_heap")
            && md.contains("20001")
            && md.contains("20000")
            && md.contains("fail"),
        "{md}"
    );
    assert!(md.contains("php_unit") && md.contains("no reading"), "{md}");

    let (ok, json, _) = status(&root, &["--section", "budget", "--format", "json"]);
    assert!(ok);
    let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
    let row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["crate"] == "php_heap")
        .unwrap();
    assert_eq!(row["ceiling"], 20000);
    assert_eq!(row["reading"]["numbers"]["lines"], 20001);
}

#[test]
fn a_section_that_has_no_renderer_yet_is_refused() {
    let root = scratch("unknown");
    let (ok, _, err) = status(&root, &["--section", "phpt"]);
    assert!(!ok);
    assert!(err.contains("phpt"), "{err}");
}
