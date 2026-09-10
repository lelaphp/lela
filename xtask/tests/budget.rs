//! `xtask budget` over the tracked workspace and the generated
//! `ext/clippy.toml` (harness-design.md §4.1, §6).

use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::budget;
use xtask::manifest::{Manifest, Scripts};
use xtask::reading::{Number, Status, Store};

mod common;

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

/// A file that is not text has no lines. A `.DS_Store` Finder left under
/// `docs/` took the whole instrument down with "stream did not contain
/// valid UTF-8", and an instrument that falls over measures nothing, which
/// is the shape of the bug #16 names: the loud failure of a sensor is still
/// a sensor that is not sensing.
#[test]
fn a_file_that_is_not_text_is_counted_as_no_lines_and_not_as_a_failure() {
    let root = scratch("not-text");
    common::scratch_workspace(workspace(), &root).unwrap();
    std::fs::copy(workspace().join("Cargo.lock"), root.join("Cargo.lock")).unwrap();
    let manifest = Manifest::load(&root).unwrap();
    let before = budget::run(&root, &manifest).unwrap();
    let docs_before = int(&workspace_reading(&before).numbers["docs"]);

    std::fs::write(root.join("docs/.DS_Store"), [0x00, 0xff, 0xfe, b'x']).unwrap();
    let after = budget::run(&root, &manifest)
        .expect("a file that is not text does not take the instrument down");
    assert_eq!(
        int(&workspace_reading(&after).numbers["docs"]),
        docs_before,
        "and counts as no lines"
    );
}

fn workspace_reading(readings: &[xtask::reading::Reading]) -> &xtask::reading::Reading {
    readings.iter().find(|r| r.scope == "workspace").unwrap()
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
    assert!(!xtask.tree().is_empty(), "stamped");

    let ws = readings.iter().find(|r| r.scope == "workspace").unwrap();
    assert_eq!(ws.status, Status::Pass, "{}", ws.message);
    for key in ["total", "bindings", "docs", "harness", "lock_packages"] {
        assert!(ws.numbers.contains_key(key), "{key}: {:?}", ws.numbers);
        assert!(
            int(&ws.numbers[key]) <= int(&ws.numbers[&format!("{key}_ceiling")]),
            "{key}"
        );
    }
    // The count is `Cargo.lock`'s `[[package]]` tables, as `cargo tree`
    // would count the default build's packages.
    let packages = std::fs::read_to_string(workspace().join("Cargo.lock"))
        .unwrap()
        .lines()
        .filter(|l| *l == "[[package]]")
        .count();
    assert_eq!(
        ws.numbers["lock_packages"],
        i64::try_from(packages).unwrap().into()
    );

    let absent = readings.iter().find(|r| r.scope == "php_heap").unwrap();
    assert_eq!(
        absent.status,
        Status::Skip,
        "a crate not yet created is skipped"
    );
}

/// A manifest without the `[budget] scripts` row, which a person writes:
/// the reading is a skip that still says what the tree measures, file by
/// file, so the number the row needs is read off it.
#[test]
fn without_the_scripts_row_the_tree_is_measured_and_held_to_nothing() {
    let root = scratch("no-row");
    common::scratch_workspace(workspace(), &root).unwrap();
    std::fs::copy(workspace().join("Cargo.lock"), root.join("Cargo.lock")).unwrap();
    std::fs::create_dir_all(root.join("scripts/hooks")).unwrap();
    std::fs::write(
        root.join("scripts/hooks/pre-commit"),
        "#!/usr/bin/env bash\nexit 0\n",
    )
    .unwrap();
    let mut manifest = Manifest::load(&root).unwrap();
    manifest.budget.scripts = None;
    let readings = budget::run(&root, &manifest).unwrap();
    let scripts = readings.iter().find(|r| r.scope == "scripts").unwrap();
    assert_eq!(scripts.status, Status::Skip, "{scripts:#?}");
    assert!(!scripts.numbers.contains_key("ceiling"));
    assert_eq!(scripts.numbers["lines"], 2.into());
    assert!(
        scripts.message.contains("no `[budget] scripts` row")
            && scripts
                .message
                .contains("made up of scripts/hooks/pre-commit 2"),
        "{}",
        scripts.message
    );
    assert!(!scripts.tree().is_empty(), "stamped over scripts/");
}

/// The mechanism, held without the row in the tracked manifest: a scratch
/// tree with a hook and a generator, the row built as the file would parse
/// it, and the generator's lines left out of the count. Everything under
/// `scripts/` counts except what the row names, and the instrument reads
/// the row rather than a list of its own (#34).
#[test]
fn the_scripts_row_names_the_generators_and_everything_else_under_scripts_counts() {
    let root = scratch("scripts");
    common::scratch_workspace(workspace(), &root).unwrap();
    std::fs::copy(workspace().join("Cargo.lock"), root.join("Cargo.lock")).unwrap();
    std::fs::create_dir_all(root.join("scripts/hooks")).unwrap();
    std::fs::write(
        root.join("scripts/hooks/pre-commit"),
        "#!/usr/bin/env bash\nexit 0\n",
    )
    .unwrap();
    std::fs::write(
        root.join("scripts/adr-index.sh"),
        "#!/usr/bin/env bash\n\nls\n",
    )
    .unwrap();
    std::fs::write(
        root.join("scripts/extract.py"),
        "import sys\n\nprint(1)\nprint(2)\nprint(3)\nprint(4)\n",
    )
    .unwrap();
    let mut manifest = Manifest::load(&root).unwrap();

    manifest.budget.scripts = Some(Scripts::new(4, &["scripts/extract.py"]));
    let readings = budget::run(&root, &manifest).unwrap();
    let scripts = readings.iter().find(|r| r.scope == "scripts").unwrap();
    assert_eq!(scripts.status, Status::Pass, "{scripts:#?}");
    assert_eq!(
        scripts.numbers["lines"],
        4.into(),
        "two and two, the generator's five left out"
    );
    assert_eq!(scripts.numbers["files"], 2.into());
    assert_eq!(scripts.numbers["ceiling"], 4.into());
    assert!(
        scripts
            .message
            .contains("made up of scripts/adr-index.sh 2, scripts/hooks/pre-commit 2")
            && scripts
                .message
                .contains("generators not counted: scripts/extract.py"),
        "{}",
        scripts.message
    );

    manifest.budget.scripts = Some(Scripts::new(4, &[]));
    let readings = budget::run(&root, &manifest).unwrap();
    let scripts = readings.iter().find(|r| r.scope == "scripts").unwrap();
    assert_eq!(
        scripts.status,
        Status::Fail,
        "a row naming no generator counts them: {scripts:#?}"
    );
    assert_eq!(scripts.numbers["lines"], 9.into());
    assert!(
        scripts.message.contains("9 lines over the ceiling of 4")
            && scripts.message.contains("scripts/extract.py 5")
            && !scripts.message.contains("generators not counted"),
        "{}",
        scripts.message
    );

    manifest.budget.scripts = Some(Scripts::new(9, &["scripts/extract_ops.py"]));
    let readings = budget::run(&root, &manifest).unwrap();
    let scripts = readings.iter().find(|r| r.scope == "scripts").unwrap();
    assert_eq!(
        scripts.status,
        Status::Fail,
        "a generator the row names that is no tracked file is a failure, not a file that quietly counts: {scripts:#?}"
    );
    assert_eq!(
        scripts.numbers["lines"],
        9.into(),
        "under the ceiling, and still a failure"
    );
    assert!(
        scripts
            .message
            .contains("names scripts/extract_ops.py as a generator and no tracked file under scripts/ is so called"),
        "{}",
        scripts.message
    );
    assert!(
        !scripts.message.contains("over the ceiling"),
        "{}",
        scripts.message
    );
}

/// The committed file is the manifest's rendering: `budget` regenerates
/// it and holds the two equal under the `generated.committed` rule, so a
/// stale copy is its failing reading and "run `xtask generate`" its repair.
#[test]
fn the_committed_clippy_toml_is_the_manifests_rendering() {
    let manifest = Manifest::load(workspace()).unwrap();
    let readings = budget::run(workspace(), &manifest).unwrap();
    let generated: Vec<&xtask::reading::Reading> = readings
        .iter()
        .filter(|r| r.rule == "generated.committed")
        .collect();
    assert_eq!(generated.len(), 1, "{generated:#?}");
    let clippy = generated[0];
    assert_eq!(clippy.scope, "ext/clippy.toml");
    assert_eq!(clippy.status, Status::Pass, "{}", clippy.message);
}

/// The rendering is read as committed, which the test above holds equal
/// to what the manifest renders.
#[test]
fn the_rendering_carries_every_denied_type_with_the_rule_text_and_adr() {
    let manifest = Manifest::load(workspace()).unwrap();
    let rule = manifest.rule("ext.no-locks").unwrap();
    let text = std::fs::read_to_string(workspace().join("ext/clippy.toml")).unwrap();
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
