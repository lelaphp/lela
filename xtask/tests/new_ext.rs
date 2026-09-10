//! `xtask new-ext` over the fixture of `xtask/fixtures/new-ext` (issue #9;
//! harness-design.md §5, design §4.10, ADR-0004, ADR-0005): the crate it
//! lays down has the template topology, passes `budget`, `unsafe-audit`
//! and `api-check` untouched, its row is rendered into the extensions
//! region of `ext/AGENTS.md`, and a second run for the same name refuses.
//! The fixture is copied to a scratch directory first; nothing is generated
//! in place, and the workspace's own `ext/` gains no crate.

use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::manifest::{Crate, Manifest};
use xtask::reading::{Reading, Status};
use xtask::{api_check, budget, new_ext, unsafe_audit, vocab};

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

/// A scratch copy of the fixture with one commit, since a reading's stamp
/// names `HEAD`.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("xtask-new-ext-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    copy(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/new-ext"),
        &dir,
    );
    // The pin is referenced by a path relative to the fixture's place in the tree.
    let seams = std::fs::read_to_string(dir.join("seams.toml")).unwrap();
    let pin = workspace().join("oracle.toml");
    std::fs::write(
        dir.join("seams.toml"),
        seams.replace("../../../oracle.toml", pin.to_str().unwrap()),
    )
    .unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@localhost",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "fixture",
        ],
    ] {
        let status = Command::new("git")
            .args(&args)
            .current_dir(&dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }
    dir
}

fn copy(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn read(root: &Path, rel: &str) -> String {
    std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

fn failed(readings: &[Reading]) -> Vec<String> {
    readings
        .iter()
        .filter(|r| r.status == Status::Fail)
        .map(|r| format!("{}: {}", r.scope, r.message))
        .collect()
}

fn of<'a>(readings: &'a [Reading], scope: &str) -> &'a Reading {
    readings
        .iter()
        .find(|r| r.scope == scope)
        .unwrap_or_else(|| panic!("no reading of scope {scope}"))
}

#[test]
fn the_crate_laid_down_has_the_template_topology() {
    let root = scratch("topology");
    let manifest = Manifest::load(&root).unwrap();
    let laid = new_ext::run(&root, &manifest, "demo").unwrap();
    assert_eq!(laid.dir, root.join("ext/demo"));

    // One dependency, the facade (ADR-0004).
    let cargo: toml::Table = toml::from_str(&read(&root, "ext/demo/Cargo.toml")).unwrap();
    assert_eq!(cargo["package"]["name"].as_str(), Some("php_ext_demo"));
    let deps = cargo["dependencies"].as_table().unwrap();
    assert_eq!(deps.len(), 1, "{deps:?}");
    assert_eq!(
        deps["php_ext"]["path"].as_str(),
        Some("../../crates/php_ext")
    );
    // The test face for tests/ alone: the same crate, its `test` feature on.
    let dev = cargo["dev-dependencies"].as_table().unwrap();
    assert_eq!(dev.len(), 1, "{dev:?}");
    assert_eq!(
        dev["php_ext"]["features"].as_array().map(Vec::len),
        Some(1),
        "{dev:?}"
    );
    assert_eq!(cargo["lints"]["workspace"].as_bool(), Some(true));

    // `extension()`, state on the value, no lock, the witness, the lints.
    let lib = read(&root, "ext/demo/src/lib.rs");
    assert!(lib.contains("pub fn extension() -> Extension"), "{lib}");
    assert!(lib.contains(".request_state::<State>()"), "{lib}");
    assert!(lib.contains("#[derive(Default)]\nstruct State"), "{lib}");
    assert!(!lib.contains("Mutex") && !lib.contains("static "), "{lib}");
    assert!(lib.contains("#![forbid(unsafe_code)]"), "{lib}");
    assert!(
        lib.contains("#![allow(rustdoc::invalid_codeblock_attributes)]"),
        "the expect fence is xtask witness's: {lib}"
    );
    for lint in manifest
        .rule("lints.no-panic")
        .unwrap()
        .predicate
        .deny_lints
        .as_ref()
        .unwrap()
    {
        assert!(
            lib.contains(lint),
            "{lint} is denied from the manifest: {lib}"
        );
    }
    assert!(lib.contains("/// ```php\n"), "{lib}");
    assert!(
        lib.contains("extension_loaded('demo')"),
        "the witness asks the oracle whether the extension is there: {lib}"
    );
    assert!(
        lib.contains("/// ```expect exit\n"),
        "the bare exit opt-in, for --accept to fill: {lib}"
    );
    assert!(
        lib.lines()
            .filter(|l| l.contains("```expect"))
            .all(|l| !l.contains("exit=")),
        "exit= is captured, never authored: {lib}"
    );
    assert!(
        lib.contains("php-src/ext/demo/tests"),
        "the phpt subset is named: {lib}"
    );

    // The note names the subset the crate's row owns, read from the
    // manifest rather than spelled a second time here (#18).
    let note = laid
        .notes
        .iter()
        .find(|n| n.starts_with("phpt subset:"))
        .unwrap_or_else(|| panic!("{:#?}", laid.notes));
    assert!(note.contains("php-src/ext/demo/tests"), "{note}");

    // Tests through the capture sink (design §4.10).
    let tests = read(&root, "ext/demo/tests/capture.rs");
    assert!(tests.contains("Capture"), "{tests}");
    assert!(tests.contains("php_ext_demo::extension()"), "{tests}");
    assert!(!tests.contains("php_bundle"), "never the bundle: {tests}");

    // The workspace member, and the generated config in place.
    let members: toml::Table = toml::from_str(&read(&root, "Cargo.toml")).unwrap();
    let members = members["workspace"]["members"].as_array().unwrap();
    assert!(
        members.iter().any(|m| m.as_str() == Some("ext/demo")),
        "{members:?}"
    );
    // `ext/clippy.toml` is rewritten as the manifest renders it, which
    // `budget` holds under `generated.committed`.
    let readings = budget::run(&root, &manifest).unwrap();
    let clippy = of(&readings, "ext/clippy.toml");
    assert_eq!(clippy.status, Status::Pass, "{}", clippy.message);

    // The row in the extensions region.
    let agents = read(&root, "ext/AGENTS.md");
    assert!(agents.contains("└── demo"), "{agents}");
    assert!(
        laid.written.iter().any(|w| w == "ext/AGENTS.md"),
        "{:?}",
        laid.written
    );

    // No word the glossary avoids, in prose or in a public name: `vocab`
    // over the laid crate, under the tracked manifest's rule and glossary,
    // with the crate's row added since the tracked manifest has none.
    std::fs::copy(workspace().join("CONTEXT.md"), root.join("CONTEXT.md")).unwrap();
    let mut tracked = Manifest::load(workspace()).unwrap();
    tracked.crates.push(Crate::named("ext/demo"));
    let readings = vocab::hold(&root, &tracked, &["ext/demo".to_owned()]).unwrap();
    let demo = of(&readings, "ext/demo");
    assert_eq!(demo.status, Status::Pass, "{}", demo.message);
    assert!(laid.dir.is_dir());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_crate_laid_down_passes_the_three_instruments_untouched() {
    let root = scratch("instruments");
    let manifest = Manifest::load(&root).unwrap();
    new_ext::run(&root, &manifest, "demo").unwrap();

    let readings = budget::run(&root, &manifest).unwrap();
    assert!(failed(&readings).is_empty(), "{:?}", failed(&readings));
    assert_eq!(of(&readings, "ext/demo").status, Status::Pass);
    assert_eq!(of(&readings, "ext/clippy.toml").status, Status::Pass);

    let readings = unsafe_audit::run(&root, &manifest).unwrap();
    assert!(failed(&readings).is_empty(), "{:?}", failed(&readings));
    assert_eq!(of(&readings, "ext/demo").status, Status::Pass);

    let readings = api_check::hold(&manifest, &root, false).unwrap();
    assert!(failed(&readings).is_empty(), "{:?}", failed(&readings));
    let demo = of(&readings, "ext/demo");
    assert_eq!(demo.status, Status::Pass, "{}", demo.message);
    assert!(
        !demo.message.contains("pub beyond extension()"),
        "nothing public beyond extension(): {}",
        demo.message
    );

    // A second engine dependency on the generated crate is outside its edges.
    let cargo = root.join("ext/demo/Cargo.toml");
    let mut text = std::fs::read_to_string(&cargo).unwrap();
    text.push_str("php_engine = { path = \"../../crates/php_engine\" }\n");
    std::fs::write(&cargo, text).unwrap();
    let readings = api_check::hold(&manifest, &root, false).unwrap();
    let demo = of(&readings, "ext/demo");
    assert_eq!(demo.status, Status::Fail, "{}", demo.message);
    assert!(demo.message.contains("php_engine"), "{}", demo.message);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_second_run_for_the_same_name_refuses_and_changes_nothing() {
    let root = scratch("twice");
    let manifest = Manifest::load(&root).unwrap();
    new_ext::run(&root, &manifest, "demo").unwrap();
    let lib = root.join("ext/demo/src/lib.rs");
    std::fs::write(&lib, "// edited after generation\n").unwrap();
    let before = read(&root, "Cargo.toml");

    let err = new_ext::run(&root, &manifest, "demo")
        .unwrap_err()
        .to_string();
    assert!(err.contains("ext/demo"), "{err}");
    assert!(err.contains("exists"), "{err}");
    assert_eq!(
        read(&root, "ext/demo/src/lib.rs"),
        "// edited after generation\n"
    );
    assert_eq!(read(&root, "Cargo.toml"), before);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_name_without_a_crate_row_refuses_naming_the_manifest() {
    let root = scratch("row");
    let manifest = Manifest::load(&root).unwrap();
    let err = new_ext::run(&root, &manifest, "nope")
        .unwrap_err()
        .to_string();
    assert!(err.contains("seams.toml"), "{err}");
    assert!(err.contains("ext/nope"), "{err}");
    assert!(!root.join("ext/nope").exists());

    let err = new_ext::run(&root, &manifest, "Demo")
        .unwrap_err()
        .to_string();
    assert!(err.contains("Demo"), "a name is lowercase: {err}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_workspace_without_the_facade_refuses() {
    let root = scratch("facade");
    std::fs::remove_dir_all(root.join("crates/php_ext")).unwrap();
    let manifest = Manifest::load(&root).unwrap();
    let err = new_ext::run(&root, &manifest, "demo")
        .unwrap_err()
        .to_string();
    assert!(err.contains("crates/php_ext"), "{err}");
    assert!(!root.join("ext/demo").exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_binary_prints_what_it_laid_down_and_exits_nonzero_on_a_refusal() {
    let root = scratch("binary");
    let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["--root", root.to_str().unwrap(), "new-ext", "demo"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("ext/demo/src/lib.rs"), "{stdout}");
    let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["--root", root.to_str().unwrap(), "new-ext", "demo"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("exists"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&root);
}
