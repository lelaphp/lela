//! The divergence registry, `docs/divergence.toml` (harness-design.md §4.3):
//! a person writes rows; `compare` or the fuzzer proposes one with status
//! `open`; the `divergence-reason` check fails an accepted row without a
//! reason; `status --section divergence` renders it and `docs/divergence.md`
//! is that rendering as a computed region.

use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::divergence::{self, Kind, Registry, Status};
use xtask::manifest::Manifest;

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

/// A scratch workspace: the tracked manifest, pin and registry, no store.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("xtask-divergence-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    for file in ["seams.toml", "oracle.toml", divergence::FILE] {
        std::fs::copy(workspace().join(file), dir.join(file)).unwrap();
    }
    dir
}

fn xtask(root: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
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

#[test]
fn the_tracked_registry_loads_with_its_rows() {
    let registry = Registry::load(workspace()).unwrap();
    assert_eq!(registry.rows.len(), 3);
    assert!(registry.rows.iter().all(|r| r.kind == Kind::Trace));
    assert!(registry.rows.iter().all(|r| r.status == Status::Accepted));
    let row = registry.row("trace:root-table-full").unwrap();
    assert!(row.reason.contains("blacklists"));
    assert!(registry.accepted("trace:root-table-full").is_some());
    assert!(registry.accepted("trace:no-such").is_none());
    let counts = registry.counts();
    assert_eq!(counts.by_kind[&Kind::Trace], 3);
    assert_eq!(counts.by_status[&Status::Accepted], 3);
    assert_eq!(counts.by_status.get(&Status::Open).copied().unwrap_or(0), 0);
}

#[test]
fn a_row_outside_the_shape_is_refused_by_name() {
    let text = r#"
[[divergence]]
id = "witness:x"
kind = "witness"
reason = "r"
status = "accepted"
since = "2026-09-07"
owner = "me"
"#;
    let err = Registry::parse(text).unwrap_err().to_string();
    assert!(err.contains("owner"), "{err}");

    let text = text.replace("kind = \"witness\"", "kind = \"golden\"");
    let text = text.replace("owner = \"me\"\n", "");
    let err = Registry::parse(&text).unwrap_err().to_string();
    assert!(err.contains("golden"), "{err}");
}

#[test]
fn a_proposed_row_is_open_with_an_empty_reason_and_keeps_the_file_s_prose() {
    let root = scratch("propose");
    let mut registry = Registry::load(&root).unwrap();
    let before = std::fs::read_to_string(root.join(divergence::FILE)).unwrap();
    let row = registry.propose("witness:deadbeef", Kind::Witness).unwrap();
    assert_eq!(row.status, Status::Open);
    assert_eq!(row.reason, "");
    assert_eq!(row.kind, Kind::Witness);

    let after = std::fs::read_to_string(root.join(divergence::FILE)).unwrap();
    assert!(
        after.starts_with(&before),
        "the file is appended to, not rewritten"
    );
    let back = Registry::load(&root).unwrap();
    assert_eq!(back.rows.len(), 4);
    assert_eq!(back.row("witness:deadbeef").unwrap().status, Status::Open);
    assert!(back.accepted("witness:deadbeef").is_none());

    let err = registry
        .propose("witness:deadbeef", Kind::Witness)
        .unwrap_err();
    assert!(err.to_string().contains("witness:deadbeef"), "{err}");
}

#[test]
fn the_reason_check_passes_the_tracked_registry() {
    let manifest = Manifest::load(workspace()).unwrap();
    let registry = Registry::load(workspace()).unwrap();
    let reading = divergence::reason_check(&registry, &manifest);
    assert_eq!(reading.status, xtask::reading::Status::Pass, "{reading:?}");
    assert_eq!(reading.instrument, divergence::CHECK);
    assert_eq!(reading.rule, "divergence.reason");
    assert_eq!(reading.numbers["rows"], 3.into());
    assert_eq!(reading.numbers["unreasoned"], 0.into());
}

/// One negative-control fixture per rule this instrument evaluates
/// (harness-design.md §4.1): every manifest rule whose `check` predicate
/// names `divergence-reason` has a fixture registry that must fail with the
/// rule's text. Rules other instruments evaluate are not this test's.
#[test]
fn every_rule_this_check_evaluates_has_a_fixture_that_fails() {
    let manifest = Manifest::load(workspace()).unwrap();
    let rules: Vec<_> = manifest
        .rules
        .iter()
        .filter(|r| r.predicate.check.as_deref() == Some(divergence::CHECK))
        .collect();
    assert!(!rules.is_empty(), "the manifest names the check");
    for rule in rules {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/rules")
            .join(format!("{}.toml", rule.id));
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!("rule {} has no fixture at {}: {e}", rule.id, path.display())
        });
        let registry = Registry::parse(&text).unwrap();
        let reading = divergence::reason_check(&registry, &manifest);
        assert_eq!(
            reading.status,
            xtask::reading::Status::Fail,
            "the fixture for {} must fail",
            rule.id
        );
        assert_eq!(reading.rule, rule.id);
        assert!(
            reading.message.contains(&rule.text),
            "the failure prints the rule's text: {}",
            reading.message
        );
        assert!(
            reading.message.contains("witness:0000000000000000"),
            "the offending row is named: {}",
            reading.message
        );
        assert!(
            !reading.message.contains("phpt:Zend/tests/fixture.phpt"),
            "a row in order is not named: {}",
            reading.message
        );
        assert_eq!(reading.numbers["unreasoned"], 1.into());
    }
}

/// A scratch workspace that is a git repository with one commit, so a
/// reading can be stamped.
fn committed(name: &str) -> PathBuf {
    let root = scratch(name);
    for args in [
        &["init", "-q"][..],
        &["-c", "user.name=x", "-c", "user.email=x@x", "add", "."],
        &[
            "-c",
            "user.name=x",
            "-c",
            "user.email=x@x",
            "commit",
            "-q",
            "-m",
            "scratch",
        ],
    ] {
        let status = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }
    root
}

#[test]
fn the_instrument_writes_a_reading_and_exits_one_on_the_fixture() {
    let root = committed("instrument");
    let (ok, out, err) = xtask(&root, &[divergence::CHECK]);
    assert!(ok, "{err}");
    assert!(out.contains("3 rows"), "{out}");
    let store = xtask::reading::Store::of_workspace(&root);
    let reading = store
        .read(divergence::CHECK, "workspace")
        .unwrap()
        .expect("the instrument wrote its reading");
    assert_eq!(reading.status, xtask::reading::Status::Pass);
    assert!(!reading.commit.is_empty(), "stamped: {reading:?}");

    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rules/divergence.reason.toml"),
        root.join(divergence::FILE),
    )
    .unwrap();
    let (ok, _, err) = xtask(&root, &[divergence::CHECK]);
    assert!(!ok);
    assert!(err.contains("witness:0000000000000000"), "{err}");
    let reading = store.read(divergence::CHECK, "workspace").unwrap().unwrap();
    assert_eq!(reading.status, xtask::reading::Status::Fail);
}

#[test]
fn status_divergence_renders_the_rows_with_counts_by_kind_and_status() {
    let root = scratch("status");
    let (ok, md, err) = xtask(&root, &["status", "--section", "divergence"]);
    assert!(ok, "{err}");
    for id in [
        "trace:nested-entry-return",
        "trace:root-table-full",
        "trace:entry-counter-typed-recv",
    ] {
        assert!(md.contains(id), "{md}");
    }
    assert!(
        md.contains("| trace | 0 | 3 | 0 | 3 |"),
        "counts by kind and status:\n{md}"
    );
    assert!(md.contains("| total | 0 | 3 | 0 | 3 |"), "{md}");
    assert!(
        md.contains("one opline earlier"),
        "the reason is rendered: {md}"
    );

    let (ok, json, _) = xtask(
        &root,
        &["status", "--section", "divergence", "--format", "json"],
    );
    assert!(ok);
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["rows"].as_array().unwrap().len(), 3);
    assert_eq!(value["rows"][0]["id"], "trace:nested-entry-return");
    assert_eq!(value["counts"]["trace"]["accepted"], 3);
    assert_eq!(value["counts"]["total"]["accepted"], 3);
    assert_eq!(value["counts"]["total"]["open"], 0);
}

#[test]
fn status_divergence_counts_a_proposed_row_as_open() {
    let root = scratch("open");
    let mut registry = Registry::load(&root).unwrap();
    registry
        .propose("phpt:Zend/tests/x.phpt", Kind::Runner)
        .unwrap();
    let (ok, md, _) = xtask(&root, &["status", "--section", "divergence"]);
    assert!(ok);
    assert!(md.contains("| runner | 1 | 0 | 0 | 1 |"), "{md}");
    assert!(md.contains("| total | 1 | 3 | 0 | 4 |"), "{md}");
}

/// `docs/divergence.md` is the view; the registry is the truth. Its region's
/// command is the status section, so its body is that rendering, which is
/// what `computed check` holds it to without running anything.
#[test]
fn docs_divergence_md_is_the_status_section_as_a_computed_region() {
    let page = std::fs::read_to_string(workspace().join("docs/divergence.md")).unwrap();
    let opener = page
        .lines()
        .find(|l| l.starts_with("<!-- computed exec"))
        .expect("one exec region");
    assert!(
        opener.contains("status --section divergence"),
        "the command is the status section: {opener}"
    );
    assert!(opener.contains("inputs=divergence.toml"), "{opener}");
    let start = page.find(opener).unwrap() + opener.len();
    let end = page[start..].find("<!-- /computed").unwrap() + start;
    let body = page[start..end].trim();
    let (ok, md, err) = xtask(workspace(), &["status", "--section", "divergence"]);
    assert!(ok, "{err}");
    assert_eq!(
        body,
        md.trim(),
        "docs/divergence.md is stale: run `computed run docs/divergence.md`"
    );
}
