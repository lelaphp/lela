//! `xtask budget` over the tracked workspace and the generated
//! `ext/clippy.toml` (harness-design.md §4.1, §6).
#![cfg(tracked)]

use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::manifest::{Budget, Manifest, Scripts, Seam};
use xtask::reading::{Number, Status, Store};
use xtask::scratch::Tree;
use xtask::{budget, generated};

mod common {
    pub mod manifest;
    pub mod repo;
    pub mod run;
}
use common::manifest::Tracked;
use common::run::xtask;

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

fn int(n: &Number) -> i64 {
    match *n {
        Number::Int(i) => i,
        Number::Float(_) => panic!("{n} is a count"),
    }
}

fn scratch(name: &str) -> Tree {
    Tree::new(&format!("budget-{name}")).unwrap()
}

/// A file that is not text has no lines. A `.DS_Store` Finder left under
/// `docs/` took the whole instrument down with "stream did not contain
/// valid UTF-8", and an instrument that falls over measures nothing, which
/// is the shape of the bug #16 names: the loud failure of a sensor is still
/// a sensor that is not sensing.
#[test]
fn a_file_that_is_not_text_is_counted_as_no_lines_and_not_as_a_failure() {
    let root = scratch("not-text");
    common::repo::scratch_workspace(workspace(), &root);
    // A manifest of no row: what is measured here is the tree.
    Tracked::minimal().write(&root);
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

/// A manifest without the `[budget] scripts` row, which a person writes:
/// the reading is a skip that still says what the tree measures, file by
/// file, so the number the row needs is read off it.
#[test]
fn without_the_scripts_row_the_tree_is_measured_and_held_to_nothing() {
    let root = scratch("no-row");
    common::repo::scratch_workspace(workspace(), &root);
    // A manifest of no row: what is measured here is the tree.
    Tracked::minimal().write(&root);
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
    assert!(!scripts.key().is_empty(), "stamped over scripts/");
}

/// A manifest that states no `bindings`, `clean_build_release_s` or
/// `api_items` under `[budget]` loads, and states no budget for them: the
/// workspace's reading carries no bindings count or ceiling, and the api
/// seam's lock no ceiling (#3187's ruling, #3207).
#[test]
fn a_budget_without_the_php_measures_holds_nothing_to_them() {
    let root = scratch("no-php-measures");
    common::repo::scratch_workspace(workspace(), &root);
    let mut stated = Budget::flat(1_000_000);
    stated.bindings = None;
    stated.clean_build_release_s = None;
    stated.api_items = None;
    let table = toml::Table::try_from(stated).unwrap();
    let manifest = Tracked::minimal()
        .with_table("budget", Some(table))
        .write(&root);
    let text = std::fs::read_to_string(root.join("seams.toml")).unwrap();
    for measure in ["bindings", "clean_build_release_s", "api_items"] {
        assert!(!text.contains(measure), "{measure} is stated: {text}");
    }
    let budget = &manifest.budget;
    let measures = (
        budget.bindings,
        budget.clean_build_release_s,
        budget.api_items,
    );
    assert_eq!(measures, (None, None, None));
    assert_eq!(manifest.lock_ceiling(&Seam::named("api", "php_ext")), None);
    std::fs::copy(workspace().join("Cargo.lock"), root.join("Cargo.lock")).unwrap();
    let readings = budget::run(&root, &manifest).unwrap();
    let numbers = &workspace_reading(&readings).numbers;
    assert!(numbers.contains_key("total"), "{numbers:?}");
    for number in ["bindings", "bindings_ceiling"] {
        assert!(!numbers.contains_key(number), "{number}: {numbers:?}");
    }
}

/// The mechanism, held without the row in the tracked manifest: a scratch
/// tree with a hook and a generator, the row built as the file would parse
/// it, and the generator's lines left out of the count. Everything under
/// `scripts/` counts except what the row names, and the instrument reads
/// the row rather than a list of its own (#34).
#[test]
fn the_scripts_row_names_the_generators_and_everything_else_under_scripts_counts() {
    let root = scratch("scripts");
    common::repo::scratch_workspace(workspace(), &root);
    // A manifest of no row: what is measured here is the tree.
    Tracked::minimal().write(&root);
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
    // The rule's own text with its ADR appended, whatever the text's
    // wording: the generator renders `<text> (ADR-<adr>)` for every row.
    assert!(
        stderr.contains("Cross-request caching is api::Cache") && stderr.contains("(ADR-0005)"),
        "the reason prints: {stderr}"
    );
    assert!(
        !stderr.contains("does not refer to a reachable type"),
        "every entry resolves: {stderr}"
    );
}

/// The fixture whose `ext.no-locks` row is the one #1781 ruled, word for
/// word: the once types beside the locks and cells, and `deny_macros`.
fn ruled_fixture() -> PathBuf {
    xtask::package_dir().join("fixtures/clippy/once-and-thread-local")
}

/// The ruled row rendered into a scratch tree beside the fixture's
/// `Cargo.lock`: the `ext/clippy.toml` it writes, and the rule.
fn render_ruled(name: &str) -> (Tree, xtask::manifest::Rule) {
    let fixture = ruled_fixture();
    let manifest = Manifest::load(&fixture).unwrap();
    let root = scratch(name);
    std::fs::copy(fixture.join("Cargo.lock"), root.join("Cargo.lock")).unwrap();
    let written = generated::write_all(&root, &manifest).unwrap();
    assert_eq!(written, [root.join("ext/clippy.toml")]);
    (root, manifest.rule("ext.no-locks").unwrap().clone())
}

/// A clippy rule's scope decides the files it lands in, and the
/// `generated` row names files and no rule (#2473): a rule added under
/// `ext/*` lands in `ext/clippy.toml`, beside the ruled row's lists and
/// named in the header, with the row unchanged.
#[test]
fn a_rule_added_under_ext_lands_in_ext_clippy_toml_without_naming_the_file() {
    let fixture = ruled_fixture();
    let text = std::fs::read_to_string(fixture.join("seams.toml")).unwrap();
    assert!(text.contains("generated = [\"ext/clippy.toml\"]"), "{text}");
    let added = Tracked::of(&fixture).with_rule("fixture.no-cells", |rule| {
        rule.extend(
            "scope = \"ext/*\"\ndeny_types = [\"std::cell::RefCell\"]\ntext = \"No RefCell in ext/* (fixture).\"\n"
                .parse::<toml::Table>()
                .unwrap(),
        );
    });
    let manifest = Manifest::parse(&added.text()).unwrap();
    let root = scratch("added-rule");
    std::fs::copy(fixture.join("Cargo.lock"), root.join("Cargo.lock")).unwrap();
    let written = generated::write_all(&root, &manifest).unwrap();
    assert_eq!(written, [root.join("ext/clippy.toml")]);
    let rendered = std::fs::read_to_string(&written[0]).unwrap();
    assert!(
        rendered.contains(
            "{ path = \"std::cell::RefCell\", reason = \"No RefCell in ext/* (fixture).\" }"
        ),
        "{rendered}"
    );
    assert!(
        rendered
            .lines()
            .next()
            .unwrap()
            .contains("fixture.no-cells")
            && rendered.lines().next().unwrap().contains("ext.no-locks"),
        "the header names every rule the file carries: {rendered}"
    );
}

/// `deny_macros` renders to `disallowed-macros` beside `deny_types`'
/// `disallowed-types`, each row with the rule's text and ADR as its
/// reason (#1888): the ruled row's `std::thread_local`, which no type ban
/// sees, and its once types among the types.
#[test]
fn deny_macros_renders_to_disallowed_macros_beside_the_types() {
    let (root, rule) = render_ruled("deny-macros");
    let text = std::fs::read_to_string(root.join("ext/clippy.toml")).unwrap();
    let reason = format!("{} (ADR-0005)", rule.text);
    let parsed: toml::Value = toml::from_str(&text).unwrap();
    let rows = |key: &str| -> Vec<(String, String)> {
        parsed[key]
            .as_array()
            .unwrap_or_else(|| panic!("{key} is a list: {text}"))
            .iter()
            .map(|row| {
                (
                    row["path"].as_str().unwrap().to_owned(),
                    row["reason"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    };
    assert_eq!(
        rows("disallowed-macros"),
        [("std::thread_local".to_owned(), reason.clone())]
    );
    let types = rows("disallowed-types");
    for denied in [
        "std::sync::Once",
        "std::sync::OnceLock",
        "std::sync::LazyLock",
        "std::cell::OnceCell",
        "std::cell::LazyCell",
        "std::sync::Mutex",
        "std::sync::atomic::AtomicUsize",
    ] {
        assert!(
            types.contains(&(denied.to_owned(), reason.clone())),
            "{denied}: {text}"
        );
    }
    assert!(
        text.contains("# once_cell::*: once_cell is not a Cargo.lock package"),
        "a package wildcard the lock lacks is said, not dropped: {text}"
    );
}

/// The rendering does what the ruled row says: clippy, with every warning
/// denied as the `clippy` instrument runs it, refuses the once type and
/// the thread-local the fixture's calls write into an extension, each
/// with the rule's text, and passes the neighbour, so no row of the file
/// is itself a warning (#1888).
#[test]
fn clippy_refuses_a_once_type_and_a_thread_local_under_the_ruled_row() {
    let (root, rule) = render_ruled("clippy-ruled");
    std::fs::copy(
        workspace().join("rust-toolchain.toml"),
        root.join("rust-toolchain.toml"),
    )
    .unwrap();
    for (call, refusal) in [
        ("once.json", Some("disallowed type `std::sync::OnceLock`")),
        (
            "thread-local.json",
            Some("disallowed macro `std::thread_local`"),
        ),
        ("allow.json", None),
    ] {
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(ruled_fixture().join(call)).unwrap())
                .unwrap();
        let name = call.trim_end_matches(".json").replace('-', "_");
        let dir = root.join("ext").join(&name);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"php_ext_{name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n[workspace]\n"),
        )
        .unwrap();
        std::fs::write(
            dir.join("src/lib.rs"),
            json["tool_input"]["content"].as_str().unwrap(),
        )
        .unwrap();
        let out = Command::new("cargo")
            .args(["clippy", "-q", "--", "-D", "warnings"])
            .current_dir(&dir)
            .env("CARGO_TARGET_DIR", root.join("target"))
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        match refusal {
            Some(refusal) => {
                assert!(!out.status.success(), "{call}: {stderr}");
                assert!(stderr.contains(refusal), "{call}: {stderr}");
                assert!(
                    stderr.contains(&rule.text),
                    "{call}: the reason prints: {stderr}"
                );
            }
            None => assert!(out.status.success(), "{call}: {stderr}"),
        }
    }
}

/// The fixture whose `fork.threads` row is the one #1950 ruled: one
/// source per denied path, named for it with `::` written `-`, and
/// `home.rs`, the call the rule allows.
fn methods_fixture() -> PathBuf {
    xtask::package_dir().join("fixtures/rules/fork.threads")
}

/// Whether the target the test runs on declares the function: libc's
/// `vfork` and nix's `daemon` are Linux's and not Apple's, and their
/// fixtures are compiled where they are declared.
fn declared_here(path: &str) -> bool {
    cfg!(target_os = "linux") || !["libc::vfork", "nix::unistd::daemon"].contains(&path)
}

/// `deny_methods` renders to `disallowed-methods` in the `clippy.toml` its
/// scope reaches, a third-party package's path `allow-invalid` since its
/// functions differ by target and feature (#1950); and clippy, under the
/// rendering and with every warning denied, refuses the call each
/// fixture makes, however it is imported, and passes the home's call
/// under its `#[expect]` and a test module's under its `allow`.
#[test]
fn clippy_refuses_each_denied_method_under_the_generated_file() {
    let fixture = methods_fixture();
    let manifest = Manifest::load(&fixture).unwrap();
    let rule = manifest.rule("fork.threads").unwrap();
    let methods = rule.predicate.deny_methods.as_ref().unwrap();
    let root = scratch("deny-methods");
    for file in ["Cargo.lock", "rust-toolchain.toml"] {
        std::fs::copy(workspace().join(file), root.join(file)).unwrap();
    }
    let written = generated::write_all(&root, &manifest).unwrap();
    assert_eq!(written, [root.join("crates/clippy.toml")]);
    let text = std::fs::read_to_string(&written[0]).unwrap();
    let parsed: toml::Value = toml::from_str(&text).unwrap();
    let rows = parsed["disallowed-methods"].as_array().unwrap();
    assert_eq!(rows.len(), methods.len(), "{text}");
    for (row, path) in rows.iter().zip(methods) {
        assert_eq!(row["path"].as_str(), Some(path.as_str()), "{text}");
        let third_party = path.starts_with("libc::") || path.starts_with("nix::");
        assert_eq!(
            row.get("allow-invalid").is_some(),
            third_party,
            "{path}: {text}"
        );
    }

    let dir = root.join("crates/fixture");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::copy(workspace().join("Cargo.lock"), dir.join("Cargo.lock")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"fork_fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n[workspace]\n\
             [dependencies]\nlibc = \"0.2\"\nnix = {{ version = \"0.31\", features = [\"process\"] }}\n\
             php_process = {{ path = {:?} }}\n",
            workspace().join("crates/php_process")
        ),
    )
    .unwrap();
    let sources: Vec<String> = methods
        .iter()
        .map(|p| format!("{}.rs", p.replace("::", "-")))
        .chain(["home.rs".to_owned()])
        .collect();
    let mut lib = Vec::new();
    for (at, source) in sources.iter().enumerate() {
        std::fs::copy(fixture.join(source), dir.join("src").join(source))
            .unwrap_or_else(|e| panic!("no fixture {source}: {e}"));
        lib.push(format!("#[path = {source:?}]\npub mod m{at};\n"));
    }
    std::fs::write(dir.join("src/lib.rs"), lib.concat()).unwrap();
    let out = Command::new("cargo")
        .args(["clippy", "-q", "--all-targets", "--", "-D", "warnings"])
        .current_dir(&dir)
        .env("CARGO_TARGET_DIR", root.join("target"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    let blocks: Vec<&str> = stderr.split("error: ").collect();
    for (path, source) in methods.iter().zip(&sources) {
        let refused = blocks.iter().any(|b| {
            b.starts_with(&format!("use of a disallowed method `{path}`"))
                && b.contains(&format!("src/{source}"))
                && b.contains(&rule.text)
        });
        assert_eq!(refused, declared_here(path), "{path} in {source}: {stderr}");
    }
    assert!(!stderr.contains("home.rs"), "the home passes: {stderr}");
    assert!(
        !stderr.contains("does not refer to a reachable"),
        "every entry resolves or is allow-invalid: {stderr}"
    );
}

/// A clippy rule clippy cannot hold as written is refused by `generate`,
/// naming the rule and why: an `exempt`, which clippy never reads, where
/// a site the rule allows carries `#[expect]`; a scope that is not a
/// directory or its children, which no one `clippy.toml` stands above;
/// and a directory the scope names with no generated file there, where
/// the rule would render to nothing.
#[test]
fn a_clippy_rule_clippy_cannot_hold_is_refused_naming_why() {
    let root = scratch("deny-methods-refused");
    std::fs::copy(workspace().join("Cargo.lock"), root.join("Cargo.lock")).unwrap();
    for (written, said) in [
        (
            "scope = \"crates/*\"\nexempt = [\"crates/php_engine/src/fork.rs\"]",
            "exempt",
        ),
        (
            "scope = \"crates/*/src/**\"",
            "not a directory or its children",
        ),
        (
            "scope = [\"crates/*\", \"sapi/*\"]",
            "no generated rule renders sapi/clippy.toml",
        ),
    ] {
        let written = Tracked::of(&methods_fixture()).with_rule("fork.threads", |rule| {
            rule.extend(written.parse::<toml::Table>().unwrap());
        });
        let manifest = Manifest::parse(&written.text()).unwrap();
        let err = format!("{:#}", generated::write_all(&root, &manifest).unwrap_err());
        assert!(err.contains("fork.threads") && err.contains(said), "{err}");
    }
}

#[test]
fn the_binary_writes_the_readings_and_exits_nonzero_on_a_failure() {
    let fixture = xtask::package_dir().join("fixtures/budget/ceiling");
    let ran = xtask(&fixture, &["budget"]);
    assert_ne!(ran.code(), 0, "{ran}");
    let stdout = ran.stdout;
    assert!(stdout.contains("fail budget php_heap"), "{stdout}");
    let store = Store::of_workspace(&fixture);
    let heap = store.read("budget", "php_heap").unwrap().unwrap();
    assert_eq!(heap.status, Status::Fail);
    std::fs::remove_dir_all(fixture.join("target")).ok();
}

/// `lock_packages` counts third-party packages, those with a `source`, and
/// never the workspace's own crates (#1335). The fixture's lock holds 161
/// of the first beside 200 of the second and fails one over its ceiling of
/// 160; the same 200 crates beside 88 third-party packages pass.
#[test]
fn lock_packages_counts_the_third_party_packages_and_not_the_workspaces_crates() {
    let fixture = xtask::package_dir().join("fixtures/budget/lock-packages");
    let ran = xtask(&fixture, &["budget"]);
    std::fs::remove_dir_all(fixture.join("target")).ok();
    assert_ne!(ran.code(), 0, "{ran}");
    let stdout = ran.stdout;
    assert!(
        stdout.contains("third-party Cargo.lock packages: 161 over the ceiling of 160"),
        "{stdout}"
    );

    let root = scratch("lock-packages");
    common::repo::scratch_workspace(workspace(), &root);
    // A manifest of no row: what is measured here is the tree.
    Tracked::minimal().write(&root);
    let lock = std::fs::read_to_string(fixture.join("Cargo.lock")).unwrap();
    let mut third_party = 0;
    let kept: Vec<&str> = lock
        .split("\n\n")
        .filter(|table| {
            if !table.contains("source = ") {
                return true;
            }
            third_party += 1;
            third_party <= 88
        })
        .collect();
    std::fs::write(root.join("Cargo.lock"), kept.join("\n\n")).unwrap();
    let mut manifest = Manifest::load(&root).unwrap();
    manifest.budget.lock_packages = 160;
    let readings = budget::run(&root, &manifest).unwrap();
    let ws = workspace_reading(&readings);
    assert_eq!(ws.numbers["lock_packages"], 88.into(), "{ws:#?}");
    assert!(
        !ws.message.contains("Cargo.lock packages"),
        "200 workspace crates beside 88 third-party packages pass: {}",
        ws.message
    );
}

/// The tests whose subject is the tracked workspace itself: each reads the
/// `seams.toml`, `oracle.toml`, registry or sources standing in the
/// workspace, so a person's edit there is what may turn one red (#3091).
/// Every other test here judges by a manifest it builds, or by a
/// scratch's byte-for-byte copy of the tracked one where the binary's
/// hook or `permit` refuses any other (`built.rs`).
mod tracked {
    use super::*;

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
        assert!(!xtask.key().is_empty(), "stamped");

        let ws = readings.iter().find(|r| r.scope == "workspace").unwrap();
        assert_eq!(ws.status, Status::Pass, "{}", ws.message);
        for key in ["total", "bindings", "docs", "harness", "lock_packages"] {
            assert!(ws.numbers.contains_key(key), "{key}: {:?}", ws.numbers);
            assert!(
                int(&ws.numbers[key]) <= int(&ws.numbers[&format!("{key}_ceiling")]),
                "{key}"
            );
        }
        // The count is `Cargo.lock`'s third-party `[[package]]` tables, those
        // with a `source` (#1335).
        let packages = std::fs::read_to_string(workspace().join("Cargo.lock"))
            .unwrap()
            .lines()
            .filter(|l| l.starts_with("source = "))
            .count();
        assert_eq!(
            ws.numbers["lock_packages"],
            i64::try_from(packages).unwrap().into()
        );

        let absent = readings.iter().find(|r| r.scope == "sapi/embed").unwrap();
        assert_eq!(
            absent.status,
            Status::Skip,
            "a crate not yet created is skipped"
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
        let scopes: Vec<&str> = generated.iter().map(|r| r.scope.as_str()).collect();
        assert_eq!(
            scopes,
            ["crates/clippy.toml", "ext/clippy.toml", "sapi/clippy.toml"],
            "{generated:#?}"
        );
        for clippy in generated {
            assert_eq!(clippy.status, Status::Pass, "{}", clippy.message);
        }
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

    /// A `deny_lints` rule's `allow_in_tests` renders as clippy's
    /// `allow-*-in-tests = true`, one key per lint, into the `clippy.toml` its
    /// scope reaches, and keeps its `exempt`, since a key only lifts a lint
    /// inside tests (#3078); the committed files carry the three keys the
    /// tracked `lints.no-panic` row lists.
    #[test]
    fn allow_in_tests_renders_clippys_in_tests_keys_beside_the_lists() {
        let fixture = ruled_fixture();
        let added = Tracked::of(&fixture).with_rule("fixture.no-panic", |rule| {
            rule.extend(
                r#"
    scope = "ext/*"
    exempt = ["ext/demo"]
    deny_lints = ["clippy::unwrap_used", "clippy::panic", "clippy::todo"]
    allow_in_tests = ["clippy::panic", "clippy::unwrap_used"]
    text = "t"
    "#
                .parse::<toml::Table>()
                .unwrap(),
            );
        });
        let manifest = Manifest::parse(&added.text()).unwrap();
        let root = scratch("allow-in-tests");
        std::fs::copy(fixture.join("Cargo.lock"), root.join("Cargo.lock")).unwrap();
        let written = generated::write_all(&root, &manifest).unwrap();
        let rendered = std::fs::read_to_string(&written[0]).unwrap();
        let parsed: toml::Table = toml::from_str(&rendered).unwrap();
        let keys: Vec<&str> = parsed
            .keys()
            .map(String::as_str)
            .filter(|k| k.ends_with("-in-tests"))
            .collect();
        assert_eq!(
            keys,
            ["allow-panic-in-tests", "allow-unwrap-in-tests"],
            "{rendered}"
        );
        assert!(parsed["disallowed-types"].is_array(), "{rendered}");

        let manifest = Manifest::load(workspace()).unwrap();
        let rule = manifest.rule("lints.no-panic").unwrap();
        assert_eq!(
            rule.predicate.allow_in_tests.as_deref().unwrap_or_default(),
            [
                "clippy::unwrap_used",
                "clippy::expect_used",
                "clippy::panic"
            ]
        );
        for file in ["crates/clippy.toml", "ext/clippy.toml", "sapi/clippy.toml"] {
            let text = std::fs::read_to_string(workspace().join(file)).unwrap();
            let parsed: toml::Table = toml::from_str(&text).unwrap();
            for key in [
                "allow-unwrap-in-tests",
                "allow-expect-in-tests",
                "allow-panic-in-tests",
            ] {
                assert_eq!(
                    parsed.get(key),
                    Some(&toml::Value::Boolean(true)),
                    "{file}: {key}"
                );
            }
        }
    }

    /// Every path a `deny_methods` rule of the workspace's manifest denies is
    /// one the fixture's row denies and has its source beside it, so a path
    /// added to the row is refused under clippy by the test above.
    #[test]
    fn every_method_the_manifest_denies_has_a_fixture() {
        let manifest = Manifest::load(workspace()).unwrap();
        let fixture = Manifest::load(&methods_fixture()).unwrap();
        let mut seen = 0;
        for rule in &manifest.rules {
            let Some(methods) = &rule.predicate.deny_methods else {
                continue;
            };
            let fixed = fixture
                .rule(&rule.id)
                .and_then(|r| r.predicate.deny_methods.as_ref());
            for path in methods {
                seen += 1;
                assert!(
                    fixed.is_some_and(|f| f.contains(path)),
                    "{}: {path} is not in the fixture's row",
                    rule.id
                );
                let source = methods_fixture().join(format!("{}.rs", path.replace("::", "-")));
                assert!(
                    source.is_file(),
                    "{}: no fixture {}",
                    rule.id,
                    source.display()
                );
            }
        }
        assert!(seen >= 8, "#1950's eight paths are denied");
    }
}
