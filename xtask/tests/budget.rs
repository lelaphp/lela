//! `xtask budget` over the tracked workspace and the generated
//! `ext/clippy.toml` (harness-design.md §4.1, §6).

use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::manifest::Manifest;
use xtask::reading::{Number, Status, Store};
use xtask::{budget, generated};

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

fn int(n: &Number) -> i64 {
    match *n {
        Number::Int(i) => i,
        Number::Float(_) => panic!("{n} is a count"),
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("xtask-budget-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn the_tracked_workspace_passes_with_the_manifests_numbers() {
    let manifest = Manifest::load(workspace()).unwrap();
    let readings = budget::run(workspace(), &manifest).unwrap();
    let failed: Vec<_> = readings
        .iter()
        .filter(|r| r.status == Status::Fail)
        .collect();
    assert!(failed.is_empty(), "{failed:#?}");

    let xtask = readings.iter().find(|r| r.scope == "xtask").unwrap();
    assert_eq!(xtask.status, Status::Pass);
    let ceiling = manifest.crate_named("xtask").unwrap().budget;
    assert_eq!(
        xtask.numbers["ceiling"],
        i64::try_from(ceiling).unwrap().into()
    );
    assert!(int(&xtask.numbers["lines"]) < int(&xtask.numbers["ceiling"]));
    assert!(!xtask.tree.is_empty(), "stamped");

    let ws = readings.iter().find(|r| r.scope == "workspace").unwrap();
    assert_eq!(ws.status, Status::Pass, "{}", ws.message);
    for key in ["total", "bindings", "docs", "harness", "lock_packages"] {
        assert!(ws.numbers.contains_key(key), "{key}: {:?}", ws.numbers);
        assert!(
            int(&ws.numbers[key]) <= int(&ws.numbers[&format!("{key}_ceiling")]),
            "{key}"
        );
    }
    assert_eq!(
        ws.numbers["lock_packages"],
        i64::try_from(generated::Lock::load(workspace()).unwrap().packages())
            .unwrap()
            .into()
    );

    let absent = readings.iter().find(|r| r.scope == "php_heap").unwrap();
    assert_eq!(
        absent.status,
        Status::Skip,
        "a crate not yet created is skipped"
    );
}

#[test]
fn the_committed_clippy_toml_is_the_manifests_rendering() {
    let manifest = Manifest::load(workspace()).unwrap();
    let rendered = generated::render_all(workspace(), &manifest).unwrap();
    assert_eq!(rendered.len(), 1, "{:?}", rendered.keys());
    let (file, want) = rendered.iter().next().unwrap();
    assert_eq!(file, "ext/clippy.toml");
    let have = std::fs::read_to_string(workspace().join(file)).unwrap();
    assert_eq!(have, *want, "run `xtask generate`");

    let readings = budget::run(workspace(), &manifest).unwrap();
    let clippy = readings.iter().find(|r| r.scope == file.as_str()).unwrap();
    assert_eq!(clippy.status, Status::Pass);
    assert_eq!(clippy.rule, "generated.committed");
}

#[test]
fn the_rendering_carries_every_denied_type_with_the_rule_text_and_adr() {
    let manifest = Manifest::load(workspace()).unwrap();
    let rule = manifest.rule("ext.no-locks").unwrap();
    let text =
        generated::render(workspace(), &manifest, "ext/clippy.toml", "ext.no-locks").unwrap();
    for ty in rule.predicate.deny_types.as_ref().unwrap() {
        if let Some(prefix) = ty.strip_suffix("::*") {
            assert!(
                text.contains(&format!("path = \"{prefix}::"))
                    || text.contains(&format!("# {ty}:")),
                "{ty} is expanded or explained: {text}"
            );
            assert!(
                !text.contains(&format!("\"{ty}\"")),
                "no wildcard reaches clippy: {text}"
            );
        } else {
            assert!(text.contains(&format!("path = \"{ty}\"")), "{ty}: {text}");
        }
    }
    assert!(
        text.contains(&rule.text),
        "the reason is the rule's text: {text}"
    );
    assert!(text.contains("(ADR-0005)"), "{text}");
    assert!(text.contains("std::sync::atomic::AtomicUsize"), "{text}");
    let parsed: toml::Value = toml::from_str(&text).unwrap();
    assert!(parsed["disallowed-types"].as_array().unwrap().len() >= 16);
}

/// The generated file does what the rule says: clippy, finding it by
/// nearest-file search from an `ext/<name>` crate, refuses a `Mutex`.
#[test]
fn clippy_refuses_a_mutex_in_an_ext_crate_under_the_generated_file() {
    let root = scratch("clippy");
    std::fs::create_dir_all(root.join("ext/fixture/src")).unwrap();
    std::fs::copy(
        workspace().join("ext/clippy.toml"),
        root.join("ext/clippy.toml"),
    )
    .unwrap();
    std::fs::copy(
        workspace().join("rust-toolchain.toml"),
        root.join("rust-toolchain.toml"),
    )
    .unwrap();
    std::fs::write(
        root.join("ext/fixture/Cargo.toml"),
        "[package]\nname = \"php_ext_fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(
        root.join("ext/fixture/src/lib.rs"),
        "pub struct State {\n    pub lock: std::sync::Mutex<u8>,\n    pub n: std::sync::atomic::AtomicUsize,\n}\n",
    )
    .unwrap();
    let out = Command::new("cargo")
        .args(["clippy", "-q", "--", "-D", "clippy::disallowed_types"])
        .current_dir(root.join("ext/fixture"))
        .env("CARGO_TARGET_DIR", root.join("target"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains("disallowed type `std::sync::Mutex`"),
        "{stderr}"
    );
    assert!(
        stderr.contains("disallowed type `std::sync::atomic::AtomicUsize`"),
        "{stderr}"
    );
    assert!(
        stderr.contains("Cross-request caching is api::Cache. (ADR-0005)"),
        "the reason prints: {stderr}"
    );
    assert!(
        !stderr.contains("does not refer to a reachable type"),
        "every entry resolves: {stderr}"
    );
}

#[test]
fn the_binary_writes_the_readings_and_exits_nonzero_on_a_failure() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/budget/ceiling");
    let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("--root")
        .arg(&fixture)
        .arg("budget")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{stdout}");
    assert!(stdout.contains("fail budget php_heap"), "{stdout}");
    let store = Store::of_workspace(&fixture);
    let heap = store.read("budget", "php_heap").unwrap().unwrap();
    assert_eq!(heap.status, Status::Fail);
    let _ = std::fs::remove_dir_all(fixture.join("target"));
}
