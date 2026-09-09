//! The negative controls of `budget` and `unsafe-audit` (harness-design.md
//! §4.1, §12 item 4): one fixture per rule these two instruments evaluate,
//! each a workspace under `xtask/fixtures/` that must fail, run through the
//! instrument's library entry. The rules are the manifest's whose
//! predicate one of the two reads: `allow` (unsafe-audit) and `generated`
//! (budget); the ceilings, which are `[[crate]]` rows and not rules, have
//! their fixtures under `xtask/fixtures/budget/`. Rules other instruments
//! evaluate have their fixtures beside those instruments.

use std::path::{Path, PathBuf};

use xtask::manifest::Manifest;
use xtask::reading::{Reading, Status};
use xtask::{budget, unsafe_audit};

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

fn fixture(kind: &str, name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(kind)
        .join(name);
    assert!(dir.is_dir(), "no fixture at {}", dir.display());
    dir
}

fn failures(readings: &[Reading]) -> Vec<&Reading> {
    readings
        .iter()
        .filter(|r| r.status == Status::Fail)
        .collect()
}

/// An instrument's library entry.
type Instrument = fn(&Path, &Manifest) -> anyhow::Result<Vec<Reading>>;

/// The predicates of the rules `budget` and `unsafe-audit` evaluate, and
/// which instrument evaluates each.
const EVALUATED: [(&str, Instrument); 2] =
    [("allow", unsafe_audit::run), ("generated", budget::run)];

#[test]
fn every_rule_the_two_instruments_evaluate_has_a_fixture_that_fails_under_it() {
    let manifest = Manifest::load(workspace()).unwrap();
    let mut seen = 0;
    for rule in &manifest.rules {
        let predicate = rule.predicate.named()[0];
        let Some((_, instrument)) = EVALUATED.iter().find(|(p, _)| *p == predicate) else {
            continue;
        };
        seen += 1;
        let root = fixture("rules", &rule.id);
        let fixture_manifest = Manifest::load(&root).unwrap();
        let readings = instrument(&root, &fixture_manifest).unwrap();
        let failed = failures(&readings);
        assert!(
            failed.iter().any(|r| r.rule == rule.id),
            "fixture {} does not fail under rule {}: {readings:#?}",
            root.display(),
            rule.id
        );
    }
    assert!(
        seen >= 2,
        "the manifest's allow and generated rules are covered"
    );
}

#[test]
fn a_crate_one_line_over_its_ceiling_fails_with_the_ceiling_and_the_design_text() {
    let root = fixture("budget", "ceiling");
    let manifest = Manifest::load(&root).unwrap();
    let readings = budget::run(&root, &manifest).unwrap();
    let heap = readings.iter().find(|r| r.scope == "php_heap").unwrap();
    assert_eq!(heap.status, Status::Fail, "{heap:#?}");
    assert_eq!(heap.numbers["lines"], 3.into());
    assert_eq!(heap.numbers["ceiling"], 2.into());
    assert!(
        heap.message.contains("3 lines over the ceiling of 2"),
        "{}",
        heap.message
    );
    assert!(heap.message.contains("design §4.1"), "{}", heap.message);
    assert!(
        heap.message.contains("size is a gate"),
        "the principle prints with the failure: {}",
        heap.message
    );
}

#[test]
fn committed_generated_code_outside_the_declared_exception_fails_the_workspace() {
    let root = fixture("budget", "generated-code");
    let manifest = Manifest::load(&root).unwrap();
    let readings = budget::run(&root, &manifest).unwrap();
    let workspace = readings.iter().find(|r| r.scope == "workspace").unwrap();
    assert_eq!(workspace.status, Status::Fail, "{workspace:#?}");
    assert!(
        workspace
            .message
            .contains("crates/php_unit/src/generated/ops.rs is committed generated code"),
        "{}",
        workspace.message
    );
}

#[test]
fn an_unsafe_block_outside_the_homes_fails_naming_the_file_and_the_rule() {
    let root = fixture("rules", "unsafe.homes");
    let manifest = Manifest::load(&root).unwrap();
    let readings = unsafe_audit::run(&root, &manifest).unwrap();
    let heap = readings.iter().find(|r| r.scope == "php_heap").unwrap();
    assert_eq!(heap.status, Status::Pass, "a home passes: {heap:#?}");
    assert_eq!(heap.numbers["sites"], 1.into());
    let pcre = readings.iter().find(|r| r.scope == "ext/pcre").unwrap();
    assert_eq!(
        pcre.status,
        Status::Pass,
        "an extension with bindings is deny with the allowance on ffi: {pcre:#?}"
    );
    assert_eq!(pcre.numbers["sites"], 1.into());
    let unit = readings.iter().find(|r| r.scope == "php_unit").unwrap();
    assert_eq!(unit.status, Status::Fail, "{unit:#?}");
    assert_eq!(unit.rule, "unsafe.homes");
    assert_eq!(unit.adr, "0006");
    assert!(
        unit.message.contains("crates/php_unit/src/lib.rs:4"),
        "the file and line: {}",
        unit.message
    );
    assert!(
        unit.message.contains("(fixture)") && unit.message.contains("ADR-0006"),
        "the rule text and its ADR: {}",
        unit.message
    );
}

#[test]
fn a_hand_edited_generated_file_fails_under_generated_committed() {
    let root = fixture("rules", "generated.committed");
    let manifest = Manifest::load(&root).unwrap();
    let readings = budget::run(&root, &manifest).unwrap();
    let clippy = readings
        .iter()
        .find(|r| r.scope == "ext/clippy.toml")
        .unwrap();
    assert_eq!(clippy.status, Status::Fail, "{clippy:#?}");
    assert_eq!(clippy.rule, "generated.committed");
    assert!(
        clippy.message.contains("differs from its rendering")
            && clippy.message.contains("(fixture)"),
        "{}",
        clippy.message
    );
}

/// An unsafe attribute, `#[unsafe(no_mangle)]`, asserts an unsafe contract
/// as a block does and is a site for the audit as it is for `permit` (#46).
/// The control is the rule's fixture with the attribute written into one
/// source outside a home and one inside, in a scratch copy: `permit` judges
/// the fixture's own sources by the workspace's homes, under which no
/// fixture source is in one, so the attribute cannot be committed into the
/// fixture until a person exempts `xtask/fixtures/**` from the rule.
#[test]
fn an_unsafe_attribute_outside_the_homes_fails_the_audit_and_inside_one_passes() {
    let root = std::env::temp_dir().join(format!(
        "xtask-fixtures-unsafe-attribute-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    copy(&fixture("rules", "unsafe.homes"), &root);
    // The fixture pins the real oracle by a path relative to its own
    // place; the copy stands elsewhere.
    let manifest_path = root.join("seams.toml");
    let manifest_text = std::fs::read_to_string(&manifest_path).unwrap().replace(
        "pin = \"../../../../oracle.toml\"",
        &format!("pin = \"{}\"", workspace().join("oracle.toml").display()),
    );
    std::fs::write(&manifest_path, manifest_text).unwrap();
    let attribute = "\n#[unsafe(no_mangle)]\npub extern \"C\" fn exported() {}\n";
    // A block whose brace stands on the line after the keyword: a site the
    // audit sees only while it reads the file whole, as `permit` does.
    let split = "pub fn split(p: *const u8) -> u8 {\n    unsafe\n    { *p }\n}\n";
    for (source, extra) in [
        ("crates/php_unit/src/lib.rs", split),
        ("ext/pcre/src/ffi/mod.rs", ""),
    ] {
        let path = root.join(source);
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(attribute);
        text.push_str(extra);
        std::fs::write(&path, text).unwrap();
    }
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["add", "-A"],
        vec![
            "-c",
            "user.name=x",
            "-c",
            "user.email=x@x",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "scratch",
        ],
    ] {
        let status = std::process::Command::new("git")
            .args(&args)
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }
    let manifest = Manifest::load(&root).unwrap();
    let readings = unsafe_audit::run(&root, &manifest).unwrap();
    let pcre = readings.iter().find(|r| r.scope == "ext/pcre").unwrap();
    assert_eq!(
        pcre.status,
        Status::Pass,
        "the attribute in a home: {pcre:#?}"
    );
    assert_eq!(
        pcre.numbers["sites"],
        2.into(),
        "the block and the attribute are two sites"
    );
    let unit = readings.iter().find(|r| r.scope == "php_unit").unwrap();
    assert_eq!(unit.status, Status::Fail, "{unit:#?}");
    assert_eq!(
        unit.numbers["sites"],
        3.into(),
        "the block, the attribute and the split block"
    );
    for line in [4, 7, 10] {
        assert!(
            unit.message
                .contains(&format!("crates/php_unit/src/lib.rs:{line}")),
            "line {line} is named: {}",
            unit.message
        );
    }
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
