//! `xtask new-ext` over the fixture of `xtask/fixtures/new-ext`
//! (harness-design.md §5, design §4.10, ADR-0004, ADR-0005): the crate it
//! lays down has the template topology, passes `budget`, `unsafe-audit`
//! and `api-check` untouched, its row is rendered into the extensions
//! region of `ext/AGENTS.md`, the bundle gains its feature and its
//! `enabled()` line (ADR-0004), and a second run for the same name
//! refuses.
//! The fixture is copied to a scratch directory first; nothing is generated
//! in place, and the workspace's own `ext/` gains no crate.
#![cfg(tracked)]

use std::path::Path;

use xtask::manifest::{Crate, Manifest};
use xtask::reading::{Reading, Status};
use xtask::scratch::Tree;
use xtask::{api_check, budget, new_ext, unsafe_audit, vocab};

mod common {
    pub mod manifest;
    pub mod repo;
    pub mod run;
}
use common::manifest::Tracked;
use common::repo::Repo;
use common::run::xtask;

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

/// A scratch copy of the fixture with one commit, since a reading's stamp
/// names `HEAD`.
fn scratch(name: &str) -> Tree {
    let dir = Tree::new(&format!("new-ext-{name}")).unwrap();
    copy(&xtask::package_dir().join("fixtures/new-ext"), &dir);
    // The fixture names the pin by a path relative to its place in the
    // tree; the copy names the workspace's.
    Tracked::of(&dir).with_the_workspace_pin().write(&dir);
    Repo::init(&dir).commit("fixture");
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
fn the_bundle_gains_the_feature_and_the_enabled_line() {
    let root = scratch("bundle-edit");
    let manifest = Manifest::load(&root).unwrap();
    let laid = new_ext::run(&root, &manifest, "demo").unwrap();

    // The optional dependency, the feature in default and in host, which
    // linked is built of and so names unchanged, and the
    // `enabled()` line behind the feature (ADR-0004); the tier's alias
    // as it was, and never in `enabled()`.
    let bundle: toml::Table = toml::from_str(&read(&root, "crates/php_bundle/Cargo.toml")).unwrap();
    assert_eq!(
        bundle["features"]["demo"]
            .as_array()
            .and_then(|a| a[0].as_str()),
        Some("dep:php_ext_demo"),
        "{bundle:?}"
    );
    for list in ["default", "host"] {
        assert!(
            bundle["features"][list]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v.as_str() == Some("demo")),
            "{list}: {bundle:?}"
        );
    }
    assert_eq!(
        bundle["features"]["linked"].as_array().map(Vec::len),
        Some(2),
        "{bundle:?}"
    );
    assert_eq!(bundle["features"]["jit"].as_array().map(Vec::len), Some(0));
    let dep = bundle["dependencies"]["php_ext_demo"].as_table().unwrap();
    assert_eq!(dep["path"].as_str(), Some("../../ext/demo"));
    assert_eq!(dep["optional"].as_bool(), Some(true));
    let text = read(&root, "crates/php_bundle/Cargo.toml");
    assert!(
        text.contains("# A name that enables nothing until php_jit exists."),
        "the edit is textual and the comments stay: {text}"
    );
    let lib = read(&root, "crates/php_bundle/src/lib.rs");
    assert!(
        lib.contains(
            "    &[\n        #[cfg(feature = \"demo\")]\n        php_ext_demo::extension,\n    ]\n"
        ),
        "{lib}"
    );
    assert!(lib.contains("pub type Tier = php_ext::Nop;"), "{lib}");
    let enabled = &lib[lib.find("fn enabled(").unwrap()..lib.find("pub type Tier").unwrap()];
    assert!(
        !enabled.contains("Nop") && !enabled.contains("Tier"),
        "the tier is not an extension (design §7.1): {enabled}"
    );
    for file in [
        "crates/php_bundle/Cargo.toml",
        "crates/php_bundle/src/lib.rs",
    ] {
        assert!(laid.written.iter().any(|w| w == file), "{:?}", laid.written);
    }
    // The two edits are printed, and nothing about the bundle is left.
    assert!(
        laid.notes
            .iter()
            .any(|n| n.starts_with("crates/php_bundle/Cargo.toml:")
                && n.contains("demo = [\"dep:php_ext_demo\"]")
                && n.contains("in default")
                && n.contains("in host")
                && n.contains("php_ext_demo as an optional dependency at ext/demo")),
        "{:#?}",
        laid.notes
    );
    assert!(
        laid.notes
            .iter()
            .any(|n| n.starts_with("crates/php_bundle/src/lib.rs:")
                && n.contains("php_ext_demo::extension")
                && n.contains("feature demo")),
        "{:#?}",
        laid.notes
    );
    assert!(
        laid.notes
            .iter()
            .filter(|n| n.starts_with("left:"))
            .all(|n| !n.contains("php_bundle")),
        "{:#?}",
        laid.notes
    );
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
    // The bundle's order holds with the one line, and the line is counted.
    let bundle = of(&readings, "php_bundle");
    assert_eq!(bundle.status, Status::Pass, "{}", bundle.message);
    assert_eq!(bundle.numbers["extensions"], 1.into(), "{}", bundle.message);

    // A second engine dependency on the generated crate is outside its edges.
    let cargo = root.join("ext/demo/Cargo.toml");
    let mut text = std::fs::read_to_string(&cargo).unwrap();
    text.push_str("php_engine = { path = \"../../crates/php_engine\" }\n");
    std::fs::write(&cargo, text).unwrap();
    let readings = api_check::hold(&manifest, &root, false).unwrap();
    let demo = of(&readings, "ext/demo");
    assert_eq!(demo.status, Status::Fail, "{}", demo.message);
    assert!(demo.message.contains("php_engine"), "{}", demo.message);
}

#[test]
fn a_second_run_for_the_same_name_refuses_and_changes_nothing() {
    let root = scratch("twice");
    let manifest = Manifest::load(&root).unwrap();
    new_ext::run(&root, &manifest, "demo").unwrap();
    let lib = root.join("ext/demo/src/lib.rs");
    std::fs::write(&lib, "// edited after generation\n").unwrap();
    let before = read(&root, "Cargo.toml");
    let bundle_before = (
        read(&root, "crates/php_bundle/Cargo.toml"),
        read(&root, "crates/php_bundle/src/lib.rs"),
    );

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
    // The refusal is before any edit: the bundle carries the line once.
    assert_eq!(read(&root, "crates/php_bundle/Cargo.toml"), bundle_before.0);
    assert_eq!(read(&root, "crates/php_bundle/src/lib.rs"), bundle_before.1);
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
}

/// A second crate is registered after the first; under `order =
/// "registration"`, the one order api-check serves, the
/// order written is php's and free, so the two reordered by hand still
/// pass.
#[test]
fn a_second_crate_is_appended_last_and_a_hand_reordering_passes_api_check() {
    let root = scratch("order");
    let manifest = Manifest::load(&root).unwrap();
    new_ext::run(&root, &manifest, "demo").unwrap();
    new_ext::run(&root, &manifest, "other").unwrap();
    let lib = read(&root, "crates/php_bundle/src/lib.rs");
    let demo = lib.find("php_ext_demo::extension").unwrap();
    let other = lib.find("php_ext_other::extension").unwrap();
    assert!(demo < other, "appended last: {lib}");
    let bundle: toml::Table = toml::from_str(&read(&root, "crates/php_bundle/Cargo.toml")).unwrap();
    assert_eq!(
        bundle["features"]["default"].as_array().map(Vec::len),
        Some(2),
        "{bundle:?}"
    );

    let demo_block = "        #[cfg(feature = \"demo\")]\n        php_ext_demo::extension,\n";
    let other_block = "        #[cfg(feature = \"other\")]\n        php_ext_other::extension,\n";
    assert!(lib.contains(&format!("{demo_block}{other_block}")), "{lib}");
    std::fs::write(
        root.join("crates/php_bundle/src/lib.rs"),
        lib.replace(
            &format!("{demo_block}{other_block}"),
            &format!("{other_block}{demo_block}"),
        ),
    )
    .unwrap();
    let readings = api_check::hold(&manifest, &root, false).unwrap();
    let bundle = of(&readings, "php_bundle");
    assert_eq!(bundle.status, Status::Pass, "{}", bundle.message);
    assert_eq!(bundle.numbers["extensions"], 2.into());
}

#[test]
fn a_workspace_without_the_bundle_refuses_before_any_edit() {
    let root = scratch("bundle");
    std::fs::remove_dir_all(root.join("crates/php_bundle")).unwrap();
    let manifest = Manifest::load(&root).unwrap();
    let before = read(&root, "Cargo.toml");
    let err = new_ext::run(&root, &manifest, "demo")
        .unwrap_err()
        .to_string();
    assert!(err.contains("crates/php_bundle"), "{err}");
    assert!(!root.join("ext/demo").exists());
    assert_eq!(read(&root, "Cargo.toml"), before);
}

#[test]
fn a_bundle_the_edit_cannot_read_refuses_before_any_edit() {
    let root = scratch("malformed");
    let cargo = root.join("crates/php_bundle/Cargo.toml");
    let text = std::fs::read_to_string(&cargo).unwrap();
    std::fs::write(&cargo, text.replace("[features]", "[not-features]")).unwrap();
    let manifest = Manifest::load(&root).unwrap();
    let before = read(&root, "Cargo.toml");
    let err = new_ext::run(&root, &manifest, "demo")
        .unwrap_err()
        .to_string();
    assert!(err.contains("[features]"), "{err}");
    assert!(!root.join("ext/demo").exists(), "nothing written");
    assert_eq!(read(&root, "Cargo.toml"), before);
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
}

#[test]
fn the_binary_prints_what_it_laid_down_and_exits_nonzero_on_a_refusal() {
    let root = scratch("binary");
    let stdout = xtask(&root, &["new-ext", "demo"]).ok();
    assert!(stdout.contains("ext/demo/src/lib.rs"), "{stdout}");
    assert!(
        stdout.contains("wrote crates/php_bundle/Cargo.toml")
            && stdout.contains("wrote crates/php_bundle/src/lib.rs"),
        "{stdout}"
    );
    assert!(
        stdout.contains("crates/php_bundle/src/lib.rs: enabled() gains php_ext_demo::extension"),
        "the two edits are printed: {stdout}"
    );
    let err = xtask(&root, &["new-ext", "demo"]).failed();
    assert!(err.contains("exists"), "{err}");
}

/// A root whose `members` name `ext/*`, as the workspace's do,
/// takes the crate in by the glob, so the binary lays it down and leaves
/// the root manifest byte for byte as it was.
#[test]
fn a_glob_member_leaves_the_root_manifest_as_it_was() {
    let root = scratch("glob");
    let text = read(&root, "Cargo.toml").replace(
        "members = [\"crates/php_ext\", \"crates/php_bundle\"]",
        "members = [\"crates/*\", \"ext/*\", \"sapi/*\", \"xtask\"]",
    );
    assert!(text.contains("\"ext/*\""), "{text}");
    std::fs::write(root.join("Cargo.toml"), &text).unwrap();
    let stdout = xtask(&root, &["new-ext", "demo"]).ok();
    assert!(stdout.contains("wrote ext/demo/Cargo.toml"), "{stdout}");
    assert!(!stdout.lines().any(|l| l == "wrote Cargo.toml"), "{stdout}");
    assert_eq!(read(&root, "Cargo.toml"), text);
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
            lib.contains("/// ```expect\n"),
            "an expect fence with no exit asserts zero, the status the snippet has: {lib}"
        );
        assert!(
            lib.lines()
                .filter(|l| l.contains("```expect"))
                .all(|l| !l.contains("exit")),
            "the crate is laid down with nothing for --accept to write, so permit over \
             the diff reads no exit= in it (witness.exit-authored): {lib}"
        );
        assert!(
            lib.contains("under `php-src/ext/demo`"),
            "the phpt subset is named: {lib}"
        );

        // The note names the subset the crate's row owns, read from the
        // manifest rather than spelled a second time here.
        let note = laid
            .notes
            .iter()
            .find(|n| n.starts_with("phpt subset:"))
            .unwrap_or_else(|| panic!("{:#?}", laid.notes));
        assert!(
            note.contains("php-src/ext/demo") && !note.contains("php-src/ext/demo/tests"),
            "the subset is the extension's directory whole (harness revision 67): {note}"
        );

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
    }
}
