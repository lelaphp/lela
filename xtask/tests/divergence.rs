//! The divergence registry, `docs/divergence.toml` (harness-design.md §4.3):
//! a person writes rows; `compare`'s caller or the fuzzer proposes one with
//! status `open`; the `divergence-reason` instrument fails an accepted row
//! without a reason; `status --section divergence` renders it and
//! `docs/divergence.md` is that rendering as a computed region.

use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::divergence::{self, Kind, Registry, Status};
use xtask::manifest::Manifest;

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

/// The negative-control fixture of one manifest rule (harness §4.1).
fn fixture(rule: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/rules")
        .join(rule)
        .join("divergence.toml")
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

/// The counts table's line for one kind, or `total`.
fn counts_line(registry: &Registry, kind: Option<Kind>) -> String {
    let counts = registry.counts();
    let of = |status: Status| match kind {
        Some(kind) => counts.cell(kind, status),
        None => counts.by_status.get(&status).copied().unwrap_or(0),
    };
    let rows = kind.map_or(u64::try_from(registry.rows.len()).unwrap(), |k| {
        counts.by_kind[&k]
    });
    format!(
        "| {} | {} | {} | {} | {rows} |",
        kind.map_or("total", Kind::as_str),
        of(Status::Open),
        of(Status::Accepted),
        of(Status::Fixed)
    )
}

#[test]
fn the_tracked_registry_loads_with_its_rows() {
    let registry = Registry::load(workspace()).unwrap();
    // The three trace rows the JIT design accepted (jit-design.md §4.1,
    // design ledger 9); a person adds the rest.
    for id in [
        "trace:nested-entry-return",
        "trace:root-table-full",
        "trace:entry-counter-typed-recv",
    ] {
        let row = registry.row(id).unwrap_or_else(|| panic!("{id}"));
        assert_eq!(row.kind, Kind::Trace);
        assert_eq!(row.status, Status::Accepted);
        assert!(registry.accepted(id).is_some());
    }
    assert!(registry.accepted("trace:no-such").is_none());
    assert!(registry.rows.iter().all(|r| !r.reason.is_empty()));
    let counts = registry.counts();
    assert_eq!(
        counts.by_kind[&Kind::Trace],
        u64::try_from(registry.rows.len()).unwrap()
    );
    assert!(counts.by_kind[&Kind::Trace] >= 3);
    assert_eq!(counts.by_status.get(&Status::Open), None);
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

    let text = text.replace("owner = \"me\"\n", "");
    assert!(Registry::parse(&text).is_ok());
    let err = Registry::parse(&text.replace("kind = \"witness\"", "kind = \"golden\""))
        .unwrap_err()
        .to_string();
    assert!(err.contains("golden"), "{err}");

    let err = Registry::parse(&text.replace("kind = \"witness\"", "kind = \"phpt\""))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("witness:x") && err.contains("phpt:"),
        "an id carries its kind's prefix: {err}"
    );
    let runner = text
        .replace("witness:x", "phpt:x")
        .replace("kind = \"witness\"", "kind = \"runner\"");
    assert!(
        Registry::parse(&runner).is_ok(),
        "runner shares phpt's prefix"
    );

    let err = Registry::parse(&format!("{text}{text}"))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("witness:x") && err.contains("two rows"),
        "{err}"
    );
}

#[test]
fn a_proposed_row_is_open_with_an_empty_reason_and_keeps_the_file_s_prose() {
    let root = scratch("propose");
    let mut registry = Registry::load(&root).unwrap();
    let before = std::fs::read_to_string(root.join(divergence::FILE)).unwrap();
    let rows = registry.rows.len();
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
    assert_eq!(back.rows.len(), rows + 1);
    assert_eq!(back.row("witness:deadbeef").unwrap().status, Status::Open);
    assert!(back.accepted("witness:deadbeef").is_none());

    let err = registry
        .propose("witness:deadbeef", Kind::Witness)
        .unwrap_err()
        .to_string();
    assert!(err.contains("witness:deadbeef"), "{err}");
    let err = registry
        .propose("witness:cafe", Kind::Trace)
        .unwrap_err()
        .to_string();
    assert!(err.contains("trace:"), "the id is held to the kind: {err}");

    let mut parsed = Registry::parse(&before).unwrap();
    let err = parsed.propose("witness:cafe", Kind::Witness).unwrap_err();
    assert!(
        err.to_string().contains(divergence::FILE),
        "a registry with no file has nothing to append to: {err}"
    );
}

#[test]
fn the_reason_instrument_passes_the_tracked_registry() {
    let manifest = Manifest::load(workspace()).unwrap();
    let registry = Registry::load(workspace()).unwrap();
    let reading = divergence::reason(&registry, &manifest);
    assert_eq!(reading.status, xtask::reading::Status::Pass, "{reading:?}");
    assert_eq!(reading.instrument, divergence::INSTRUMENT);
    assert_eq!(reading.rule, "divergence.reason");
    assert_eq!(
        reading.numbers["rows"],
        xtask::reading::Number::count(registry.rows.len())
    );
    assert_eq!(reading.numbers["unreasoned"], 0.into());
}

/// One negative-control fixture per rule this instrument evaluates
/// (harness-design.md §4.1): every manifest rule whose `check` predicate
/// names `divergence-reason` has a fixture registry under
/// `xtask/fixtures/rules/<id>/` that must fail with the rule's text. Rules
/// other instruments evaluate are not this test's.
#[test]
fn every_rule_this_instrument_evaluates_has_a_fixture_that_fails() {
    let manifest = Manifest::load(workspace()).unwrap();
    let rules: Vec<_> = manifest
        .rules
        .iter()
        .filter(|r| r.predicate.check.as_deref() == Some(divergence::INSTRUMENT))
        .collect();
    assert!(!rules.is_empty(), "the manifest names the instrument");
    for rule in rules {
        let path = fixture(&rule.id);
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!("rule {} has no fixture at {}: {e}", rule.id, path.display())
        });
        let registry = Registry::parse(&text).unwrap();
        let reading = divergence::reason(&registry, &manifest);
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

#[test]
fn the_instrument_writes_a_reading_and_exits_one_on_the_fixture() {
    let root = committed("instrument");
    let (ok, out, err) = xtask(&root, &[divergence::INSTRUMENT]);
    assert!(ok, "{err}");
    assert!(out.contains("every accepted row has a reason"), "{out}");
    let store = xtask::reading::Store::of_workspace(&root);
    let reading = store
        .read(divergence::INSTRUMENT, "workspace")
        .unwrap()
        .expect("the instrument wrote its reading");
    assert_eq!(reading.status, xtask::reading::Status::Pass);
    assert!(!reading.commit.is_empty(), "stamped: {reading:?}");

    std::fs::copy(fixture("divergence.reason"), root.join(divergence::FILE)).unwrap();
    let (ok, _, err) = xtask(&root, &[divergence::INSTRUMENT]);
    assert!(!ok);
    assert!(err.contains("witness:0000000000000000"), "{err}");
    let reading = store
        .read(divergence::INSTRUMENT, "workspace")
        .unwrap()
        .unwrap();
    assert_eq!(reading.status, xtask::reading::Status::Fail);
}

#[test]
fn status_divergence_renders_the_rows_with_counts_by_kind_and_status() {
    let root = scratch("status");
    let registry = Registry::load(&root).unwrap();
    let (ok, md, err) = xtask(&root, &["status", "--section", "divergence"]);
    assert!(ok, "{err}");
    for row in &registry.rows {
        assert!(md.contains(&row.id), "{md}");
        assert!(md.contains(&row.reason), "the reason is rendered: {md}");
    }
    assert!(md.contains("trace:nested-entry-return"), "{md}");
    assert!(
        md.contains(&counts_line(&registry, Some(Kind::Trace))),
        "counts by kind and status:\n{md}"
    );
    assert!(md.contains(&counts_line(&registry, None)), "{md}");

    let (ok, json, _) = xtask(
        &root,
        &["status", "--section", "divergence", "--format", "json"],
    );
    assert!(ok);
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["rows"].as_array().unwrap().len(), registry.rows.len());
    assert_eq!(value["rows"][0]["id"], registry.rows[0].id);
    assert_eq!(
        value["counts"]["trace"]["accepted"],
        registry.counts().cell(Kind::Trace, Status::Accepted)
    );
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
    assert!(md.contains(&counts_line(&registry, None)), "{md}");
    assert!(md.contains("| total | 1 | "), "{md}");
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
