//! The divergence registry, `docs/divergence.toml` (harness-design.md §4.3):
//! a person writes rows; `compare`'s caller or the fuzzer proposes one with
//! status `open`; the `divergence-reason` instrument fails an accepted row
//! without a reason; `status --section divergence` renders it and
//! `docs/divergence.md` is that rendering as a computed region.
#![cfg(tracked)]

use std::path::{Path, PathBuf};

use xtask::divergence::{self, Cause, Digests, Kind, Registry, Release, Retires, Status};
use xtask::manifest::Manifest;
use xtask::oracle::Pin;
use xtask::scratch::Tree;
use xtask::sha256::Sha256;

mod common {
    pub mod manifest;
    pub mod registry;
    pub mod repo;
    pub mod run;
}
use common::manifest::Tracked;
use common::registry::Row;
use common::repo::Repo;
use common::run::xtask;

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

/// The negative-control fixture of one manifest rule (harness §4.1).
fn fixture(rule: &str) -> PathBuf {
    xtask::package_dir()
        .join("fixtures/rules")
        .join(rule)
        .join("divergence.toml")
}

/// A scratch workspace: a manifest of the guard's rule alone (as
/// [`reasoned`] states it), the tracked pin, [`REGISTRY`] as its registry,
/// no store.
fn scratch(name: &str) -> Tree {
    let dir = Tree::new(&format!("divergence-{name}")).unwrap();
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    std::fs::copy(workspace().join("oracle.toml"), dir.join("oracle.toml")).unwrap();
    Tracked::minimal()
        .with_rule(REASON, |rule| {
            rule.extend(REASON_ROW.parse::<toml::Table>().unwrap());
        })
        .write(&dir);
    std::fs::write(dir.join(divergence::FILE), REGISTRY).unwrap();
    dir
}

/// The registry a scratch starts from, in place of the tracked one, whose
/// rows a person keeps: prose above the rows, as the tracked file
/// carries, an accepted debt row and an accepted row of another cause,
/// each reasoned as the guard holds them, and no open row.
const REGISTRY: &str = r#"# The divergence registry of a scratch workspace (fixture).

[[divergence]]
id = "witness:fixture-accepted"
kind = "witness"
reason = "The fixture's accepted reason (fixture)."
status = "accepted"
since = "2026-09-07"

[[divergence]]
id = "phpt:fixture-chosen-here"
kind = "phpt"
cause = "by-design"
reason = "The fixture's chosen difference, as design §3.1 chose it (fixture)."
status = "accepted"
since = "2026-09-08"
"#;

/// A manifest of the one rule this instrument evaluates, with a text of
/// the test's own: what a test of the guard's mechanics judges by, where
/// the tracked rule is held to its fixtures in [`tracked`].
fn reasoned() -> Manifest {
    let row = Tracked::minimal().with_rule(REASON, |rule| {
        rule.extend(REASON_ROW.parse::<toml::Table>().unwrap());
    });
    Manifest::parse(&row.text()).unwrap()
}

/// The rule this instrument evaluates, by its id.
const REASON: &str = "divergence.reason";

/// [`REASON`]'s row as a test states it, its text the test's own.
const REASON_ROW: &str = r#"
scope = "docs/divergence.toml"
check = "divergence-reason"
text = "An accepted row has a reason a person wrote (fixture)."
"#;

/// A scratch workspace that is a git repository with one commit, so a
/// reading can be stamped.
fn committed(name: &str) -> Tree {
    let root = scratch(name);
    Repo::init(&root).commit("scratch");
    root
}

/// The counts table's line for the debt rows of one kind, or `total`,
/// every debt row's.
fn counts_line(registry: &Registry, kind: Option<Kind>) -> String {
    let counts = registry.counts();
    let of = |status: Status| match kind {
        Some(kind) => counts.debt(kind, status),
        None => counts.total(status),
    };
    let rows: u64 = Status::ALL.into_iter().map(of).sum();
    format!(
        "| {} | {} | {} | {} | {rows} |",
        kind.map_or("total", Kind::as_str),
        of(Status::Open),
        of(Status::Accepted),
        of(Status::Fixed)
    )
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

    // A row pins both sides' lines or neither, each a digest as the file
    // spells one, and its digests serialise back as they were read.
    let oracle = format!("oracle = \"{}\"\n", Sha256::of(b"php"));
    let engine = format!("engine = \"{}\"\n", Sha256::of(b"engine"));
    let pinned = Registry::parse(&format!("{text}{oracle}{engine}")).unwrap();
    let row = &pinned.rows()[0];
    assert_eq!(row.digests, Some(Digests::of(b"php", b"engine")));
    let written = toml::to_string(row).unwrap();
    assert!(
        written.contains(&oracle) && written.contains(&engine),
        "{written}"
    );
    let err = format!(
        "{:#}",
        Registry::parse(&format!("{text}{engine}")).unwrap_err()
    );
    assert!(
        err.contains("witness:x names one side's digest and not the other's"),
        "{err}"
    );
    let err = format!(
        "{:#}",
        Registry::parse(&format!("{text}oracle = \"sha256:1\"\n")).unwrap_err()
    );
    assert!(err.contains("is not a digest spelt sha256:"), "{err}");
}

#[test]
fn a_proposed_row_is_open_with_an_empty_reason_and_keeps_the_file_s_prose() {
    let root = scratch("propose");
    let mut registry = Registry::load(&root).unwrap();
    let before = std::fs::read_to_string(root.join(divergence::FILE)).unwrap();
    let rows = registry.rows().len();
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
    assert_eq!(back.rows().len(), rows + 1);
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
fn the_instrument_writes_a_reading_and_exits_one_on_the_fixture() {
    let root = committed("instrument");
    let out = xtask(&root, &[divergence::INSTRUMENT]).ok();
    assert!(out.contains("every accepted row has a reason"), "{out}");
    let store = xtask::reading::Store::of_workspace(&root);
    let reading = store
        .read(divergence::INSTRUMENT, "workspace")
        .unwrap()
        .expect("the instrument wrote its reading");
    assert_eq!(reading.status, xtask::reading::Status::Pass);
    assert!(!reading.commit.is_empty(), "stamped: {reading:?}");

    std::fs::copy(fixture("divergence.reason"), root.join(divergence::FILE)).unwrap();
    let err = xtask(&root, &[divergence::INSTRUMENT]).failed();
    assert!(err.contains("witness:0000000000000000"), "{err}");
    let reading = store
        .read(divergence::INSTRUMENT, "workspace")
        .unwrap()
        .unwrap();
    assert_eq!(reading.status, xtask::reading::Status::Fail);
}

/// The scratch's registry and the divergence section rendered over it
/// with `args` after `--section divergence`.
fn rendered(name: &str, args: &[&str]) -> (Registry, String) {
    let root = scratch(name);
    let registry = Registry::load(&root).expect("load the scratch's registry");
    let section = [&["status", "--section", "divergence"][..], args].concat();
    let text = xtask(&root, &section).ok();
    (registry, text)
}

/// The kind of a debt row the registry holds, whichever rows it keeps.
fn debt_kind(registry: &Registry) -> Kind {
    registry
        .rows()
        .iter()
        .find(|r| r.cause == Cause::Debt)
        .expect("the scratch's registry has a debt row")
        .kind
}

/// That `text` carries the counts of one kind of debt row the registry
/// holds and the total.
fn assert_counts(registry: &Registry, text: &str) {
    let kind = debt_kind(registry);
    assert!(
        text.contains(&counts_line(registry, Some(kind))),
        "counts by kind and status:\n{text}"
    );
    assert!(text.contains(&counts_line(registry, None)), "{text}");
}

/// `--uncut` renders every row with its reason whole, which is where an
/// accepted row's reason is read, beside the counts by kind and status,
/// and names no command for the rest.
#[test]
fn the_uncut_section_renders_every_row_with_its_reason_whole() {
    let (registry, md) = rendered("status-uncut", &["--uncut"]);
    assert!(
        registry.rows().iter().any(|r| r.status == Status::Accepted),
        "the scratch's registry has an accepted row to read"
    );
    for row in registry.rows() {
        assert!(md.contains(row.id.as_str()), "{md}");
        // A multi-line reason renders on one line of the table, so the
        // newline TOML leaves at its end is a space here.
        assert!(
            md.contains(&row.reason.replace('\n', " ")),
            "the reason is rendered: {md}"
        );
    }
    assert_counts(&registry, &md);
    assert!(!md.contains("--uncut"), "{md}");
}

/// Without `--uncut` the section is what the local facts carry: the
/// counts and the `open` rows alone, naming the command that reads every
/// reason.
#[test]
fn the_cut_section_lists_the_open_rows_alone_and_names_the_uncut_command() {
    let (registry, cut) = rendered("status-cut", &[]);
    for row in registry.rows() {
        assert_eq!(
            cut.contains(&format!("| {} ", row.id.as_str())),
            row.status == Status::Open,
            "the facts list the open rows alone: {cut}"
        );
    }
    assert_counts(&registry, &cut);
    assert!(
        cut.contains("`cargo xtask status --section divergence --uncut`"),
        "the facts name the command that reads every reason: {cut}"
    );
}

/// `--format json` carries every row, in the registry's order, and the
/// counts by kind and status the tables render.
#[test]
fn the_json_section_carries_every_row_and_the_counts() {
    let (registry, json) = rendered("status-json", &["--format", "json"]);
    let value: serde_json::Value = serde_json::from_str(&json).expect("the section's JSON parses");
    let kind = debt_kind(&registry);
    assert_eq!(
        value["rows"].as_array().map(Vec::len),
        Some(registry.rows().len()),
        "{json}"
    );
    assert_eq!(value["rows"][0]["id"], registry.rows()[0].id.as_str());
    assert_eq!(
        value["counts"][kind.as_str()]["accepted"],
        registry.counts().debt(kind, Status::Accepted)
    );
    assert_eq!(
        value["counts"]["total"]["open"],
        registry.counts().total(Status::Open)
    );
}

/// One more accepted row changes the counts and nothing else of what
/// the facts carry: its id and reason are read under `--uncut`.
#[test]
fn one_more_accepted_row_changes_the_cut_section_s_counts_alone() {
    let root = scratch("status-grown");
    let cut = xtask(&root, &["status", "--section", "divergence"]).ok();
    let path = root.join(divergence::FILE);
    let text = std::fs::read_to_string(&path).expect("read the scratch's registry");
    let row = Row::accepted("trace:fixture-one-more")
        .reason("The fixture's one more reason (fixture).")
        .toml();
    std::fs::write(&path, format!("{text}\n{row}")).expect("write the grown registry");
    let grown = xtask(&root, &["status", "--section", "divergence"]).ok();
    let beside_counts = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|l| !l.starts_with("| trace |") && !l.starts_with("| total |"))
            .map(str::to_owned)
            .collect()
    };
    let registry = Registry::load(&root).expect("load the grown registry");
    for counts in [
        counts_line(&registry, Some(Kind::Trace)),
        counts_line(&registry, None),
    ] {
        assert!(
            grown.contains(&counts),
            "the grown counts, {counts}:\n{grown}"
        );
        assert!(
            !cut.contains(&counts),
            "counts the row moved, {counts}:\n{cut}"
        );
    }
    assert_eq!(beside_counts(&grown), beside_counts(&cut), "{grown}");
    let added = registry
        .rows()
        .iter()
        .find(|r| r.id.as_str() == "trace:fixture-one-more")
        .expect("the grown registry holds the added row");
    let whole = xtask(&root, &["status", "--section", "divergence", "--uncut"]).ok();
    let line = format!(
        "| trace:fixture-one-more | trace | debt | accepted | {} | The fixture's one more reason (fixture). |",
        added.since
    );
    assert!(whole.contains(&line), "{whole}");
}

/// The `crosscheck` kind: `parse` accepts a row keyed by an
/// extension's name under `crosscheck:`, holds the id to that prefix, and
/// `status --section divergence` renders the kind's row of counts with the
/// rest.
#[test]
fn a_crosscheck_row_parses_and_status_counts_its_kind() {
    let row = r#"
[[divergence]]
id = "crosscheck:ctype"
kind = "crosscheck"
reason = "The fixture's one byte (fixture)."
status = "accepted"
since = "2026-09-18"
"#;
    let parsed = Registry::parse(row).unwrap();
    assert_eq!(parsed.rows()[0].kind, Kind::Crosscheck);
    assert_eq!(Kind::Crosscheck.prefix(), "crosscheck:");
    assert_eq!(Kind::Crosscheck.as_str(), "crosscheck");
    assert!(parsed.accepted("crosscheck:ctype").is_some());
    assert_eq!(Kind::ALL.len(), 8);
    assert_eq!(
        Kind::ALL.iter().position(|k| *k == Kind::Crosscheck),
        Kind::ALL
            .iter()
            .position(|k| *k == Kind::Compile)
            .map(|i| i + 1),
        "after compile, in the registry's order"
    );
    let err = Registry::parse(&row.replace("crosscheck:ctype", "ext/ctype"))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("ext/ctype") && err.contains("crosscheck:"),
        "the id carries the kind's prefix: {err}"
    );

    // The fixture's row is the registry's one row, whatever rows the
    // workspace's own registry carries.
    let root = scratch("crosscheck");
    std::fs::write(root.join(divergence::FILE), row).unwrap();
    let registry = Registry::load(&root).unwrap();
    let md = xtask(&root, &["status", "--section", "divergence", "--uncut"]).ok();
    assert!(
        md.contains("| crosscheck:ctype | crosscheck | debt | accepted |"),
        "{md}"
    );
    assert!(
        md.contains(&counts_line(&registry, Some(Kind::Crosscheck))),
        "the kind's row of counts:\n{md}"
    );
    assert!(md.contains("| crosscheck | 0 | 1 | 0 | 1 |"), "{md}");
    assert!(md.contains(&counts_line(&registry, None)), "{md}");
    let json = xtask(
        &root,
        &["status", "--section", "divergence", "--format", "json"],
    )
    .ok();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["counts"]["crosscheck"]["accepted"], 1);
}

/// The `wire` kind: a difference on the wire, held to the RFCs
/// through a conformance suite rather than to the oracle, keyed by the
/// suite and the case, `wire:<suite>/<case>`, the case free to carry
/// slashes of its own as h2spec's `http2/6.5.3/1` does. An id without a
/// suite or a case is refused, and `status` counts the kind with the rest.
#[test]
fn a_wire_row_is_keyed_by_suite_and_case_and_status_counts_its_kind() {
    let row = r#"
[[divergence]]
id = "wire:h2spec/http2/6.5.3/1"
kind = "wire"
reason = "The fixture's frame (fixture)."
status = "accepted"
since = "2026-10-10"
"#;
    let parsed = Registry::parse(row).unwrap();
    assert_eq!(parsed.rows()[0].kind, Kind::Wire);
    assert_eq!(Kind::Wire.prefix(), "wire:");
    assert_eq!(Kind::ALL[6], Kind::Wire);
    assert!(parsed.accepted("wire:h2spec/http2/6.5.3/1").is_some());
    for bad in ["wire:h2spec", "wire:/6.5.3", "wire:h2spec/", "h2spec/6.5.3"] {
        let err = Registry::parse(&row.replace("wire:h2spec/http2/6.5.3/1", bad))
            .unwrap_err()
            .to_string();
        assert!(err.contains(bad) && err.contains("wire:"), "{bad}: {err}");
    }
    assert!(divergence::Id::of(Kind::Wire, "wire:rfc9112/cl-te").is_ok());
    assert!(divergence::Id::of(Kind::Wire, "wire:rfc9112").is_err());

    let root = scratch("wire");
    std::fs::write(root.join(divergence::FILE), row).unwrap();
    let md = xtask(&root, &["status", "--section", "divergence", "--uncut"]).ok();
    assert!(
        md.contains("| wire:h2spec/http2/6.5.3/1 | wire | debt | accepted |"),
        "{md}"
    );
    assert!(md.contains("| wire | 0 | 1 | 0 | 1 |"), "{md}");
}

/// The `makala` kind: one test where the production server
/// differs from php's SAPIs by design, keyed by its path in php-src, which
/// the php-fpm comparison and the served phpt lane both consult through
/// [`Registry::makala`]; a row of a kind the registry does not name is
/// refused by that kind.
#[test]
fn a_makala_row_is_keyed_by_its_test_and_an_unknown_kind_is_refused() {
    let row = r#"
[[divergence]]
id = "makala:ext/filter/tests/004.phpt"
kind = "makala"
cause = "by-design"
reason = "The fixture's refusal (third-engine-design.md §4.8)."
status = "accepted"
since = "2026-10-11"
covers = ["makala:ext/filter/tests/007.phpt"]
"#;
    let parsed = Registry::parse(row).unwrap();
    assert_eq!(parsed.rows()[0].kind, Kind::Makala);
    assert_eq!(
        (Kind::Makala.as_str(), Kind::Makala.prefix()),
        ("makala", "makala:")
    );
    assert_eq!(Kind::ALL.len(), 8);
    assert_eq!(Kind::ALL.last(), Some(&Kind::Makala));
    for test in ["ext/filter/tests/004.phpt", "ext/filter/tests/007.phpt"] {
        let found = parsed.makala(test).map(|r| r.id.as_str());
        assert_eq!(found, Some("makala:ext/filter/tests/004.phpt"), "{test}");
    }
    assert!(parsed.makala("ext/filter/tests/011.phpt").is_none());
    let open = Registry::parse(&row.replace("accepted", "open")).unwrap();
    assert!(
        open.makala("ext/filter/tests/004.phpt").is_none(),
        "an open row accepts nothing"
    );
    let err = Registry::parse(&row.replace("kind = \"makala\"", "kind = \"fpm\""))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("unknown kind \"fpm\"") && err.contains("makala"),
        "{err}"
    );
}

/// A row's `cause` (design revision 124): `debt` where the file writes
/// none, and the cause it names otherwise, each of the four by its
/// spelling; a cause outside them is refused by name.
#[test]
fn a_row_without_a_cause_is_debt_and_a_row_with_one_reads_it() {
    let row = |cause: &str| {
        format!(
            "[[divergence]]\nid = \"trace:fixture\"\nkind = \"trace\"\n{cause}reason = \"r §1\"\nstatus = \"accepted\"\nsince = \"2026-10-09\"\nretires = \"pin\"\n"
        )
    };
    let without = Registry::parse(&row("")).unwrap();
    assert_eq!(without.rows()[0].cause, Cause::Debt);
    for cause in Cause::ALL {
        let parsed = Registry::parse(&row(&format!("cause = \"{}\"\n", cause.as_str()))).unwrap();
        assert_eq!(parsed.rows()[0].cause, cause);
    }
    assert_eq!(
        Cause::ALL.map(Cause::as_str),
        ["debt", "php-ub", "by-design", "oracle-build"]
    );
    let err = Registry::parse(&row("cause = \"taste\"\n"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("taste"), "{err}");
}

/// The kind a row of php's undefined behaviour once had, `php-ub`, which
/// became a cause and which the migration moved off the registry, is
/// refused as any unknown kind is, naming the kind and the row; the same
/// row as the migration spelt it, `trace:` with the cause written,
/// decodes.
#[test]
fn a_row_of_the_old_php_ub_kind_is_refused_naming_the_kind() {
    let text = r#"
[[divergence]]
id = "php-ub:fixture-freed-table"
kind = "php-ub"
reason = "The fixture's undefined behaviour (fixture)."
status = "accepted"
since = "2026-10-07"
covers = ["php-ub:ext/standard/tests/walks.rs::fixture_walk_ends"]
"#;
    let err = format!("{:#}", Registry::parse(text).unwrap_err());
    assert!(err.contains("unknown kind \"php-ub\""), "{err}");
    assert!(err.contains("php-ub:fixture-freed-table"), "{err}");

    let migrated = text
        .replace("php-ub:", "trace:")
        .replace("kind = \"php-ub\"", "kind = \"trace\"\ncause = \"php-ub\"");
    let row = Registry::parse(&migrated).unwrap().rows()[0].clone();
    assert_eq!((row.kind, row.cause), (Kind::Trace, Cause::PhpUb));
}

/// `retires` (Mitchell, 2026-10-09) is one of four values, and any other
/// spelling is refused when the registry decodes.
#[test]
fn retires_decodes_as_one_of_four_values_and_an_unknown_one_is_refused() {
    let row = |retires: &str| {
        format!(
            "[[divergence]]\nid = \"crosscheck:openssl\"\nkind = \"crosscheck\"\ncause = \"oracle-build\"\nretires = \"{retires}\"\nreason = \"r\"\nstatus = \"accepted\"\nsince = \"2026-10-09\"\n"
        )
    };
    for retires in [
        Retires::OracleRebuild,
        Retires::Pin,
        Retires::Fixture,
        Retires::Platform,
    ] {
        let parsed = Registry::parse(&row(retires.as_str())).unwrap();
        assert_eq!(parsed.rows()[0].retires, Some(retires));
    }
    let err = Registry::parse(&row("someday")).unwrap_err().to_string();
    assert!(err.contains("someday"), "{err}");
    let none = row("pin").replace("retires = \"pin\"\n", "");
    assert_eq!(Registry::parse(&none).unwrap().rows()[0].retires, None);
}

/// One row of each cause, a scratch workspace's registry: `status
/// --section divergence` counts the debt row above the total, holds the
/// total to the manifest's `[divergence] target`, and counts each other
/// cause on a line of its own after it.
const ONE_OF_EACH: &str = r#"
[[divergence]]
id = "trace:fixture-debt"
kind = "trace"
reason = "The fixture's parity reason (fixture)."
status = "accepted"
since = "2026-09-29"

[[divergence]]
id = "trace:fixture-freed-table"
kind = "trace"
cause = "php-ub"
reason = "The fixture's undefined behaviour (fixture)."
status = "accepted"
since = "2026-10-07"

[[divergence]]
id = "phpt:fixture-chosen"
kind = "phpt"
cause = "by-design"
reason = "Chosen by design §5.2 (fixture)."
status = "accepted"
since = "2026-10-08"

[[divergence]]
id = "crosscheck:fixture"
kind = "crosscheck"
cause = "oracle-build"
retires = "oracle-rebuild"
reason = "The oracle's build (fixture)."
status = "open"
since = "2026-10-09"
"#;

#[test]
fn status_counts_debt_against_the_manifests_target_and_each_other_cause_on_its_own_line() {
    let root = scratch("causes");
    std::fs::write(root.join(divergence::FILE), ONE_OF_EACH).unwrap();
    let target = |target: Option<i64>| {
        let table = target.map(|n| toml::Table::from_iter([("target".to_owned(), n.into())]));
        Tracked::of(&root)
            .with_table("divergence", table)
            .write(&root);
    };
    target(Some(7));
    let md = xtask(&root, &["status", "--section", "divergence", "--uncut"]).ok();
    let lines: Vec<&str> = md.lines().collect();
    let at = |prefix: &str| {
        lines
            .iter()
            .position(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("no line {prefix}: {md}"))
    };
    assert_eq!(lines[at("| trace |")], "| trace | 0 | 1 | 0 | 1 |", "{md}");
    assert_eq!(lines[at("| total |")], "| total | 0 | 1 | 0 | 1 |", "{md}");
    assert_eq!(
        lines[at("| php-ub |")],
        "| php-ub | 0 | 1 | 0 | 1 |",
        "{md}"
    );
    assert_eq!(
        lines[at("| by-design |")],
        "| by-design | 0 | 1 | 0 | 1 |",
        "{md}"
    );
    assert_eq!(
        lines[at("| oracle-build |")],
        "| oracle-build | 1 | 0 | 0 | 1 |",
        "{md}"
    );
    assert!(
        at("| trace |") < at("| total |")
            && at("| total |") < at("| php-ub |")
            && at("| php-ub |") < at("| by-design |")
            && at("| by-design |") < at("| oracle-build |"),
        "{md}"
    );
    assert!(
        !lines.iter().any(|l| l.starts_with("| phpt |")),
        "no debt row is a phpt row: {md}"
    );
    assert!(md.contains("day-30 target of under 7"), "{md}");
    assert!(md.contains("counted on its own line"), "{md}");
    assert!(
        md.contains("| phpt:fixture-chosen | phpt | by-design | accepted |"),
        "{md}"
    );
    let json = xtask(
        &root,
        &["status", "--section", "divergence", "--format", "json"],
    )
    .ok();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["target"], 7);
    assert_eq!(value["counts"]["total"]["accepted"], 1);
    assert_eq!(value["counts"]["php-ub"]["accepted"], 1);
    assert_eq!(value["counts"]["oracle-build"]["open"], 1);
    let rows = value["rows"].as_array().unwrap();
    assert_eq!(rows[3]["cause"], "oracle-build");
    assert_eq!(rows[3]["retires"], "oracle-rebuild");

    // Without the table the total is held to nothing, and says so; with
    // debt rows alone no line is about another cause.
    target(None);
    let debt_only = ONE_OF_EACH.split("\n[[divergence]]").nth(1).unwrap();
    std::fs::write(
        root.join(divergence::FILE),
        format!("[[divergence]]{debt_only}"),
    )
    .unwrap();
    let md = xtask(&root, &["status", "--section", "divergence"]).ok();
    assert!(md.contains("no `[divergence] target`"), "{md}");
    assert!(!md.contains("counted on its own line"), "{md}");
    assert!(md.contains("| total | 0 | 1 | 0 | 1 |"), "{md}");
}

/// `fixed_in` (ruled 2026-11-10) is a php release spelt
/// `<major>.<minor>.<patch>`, ordered by its numbers; any other spelling
/// is refused when the registry decodes, and a row writes it back as it
/// was read.
#[test]
fn fixed_in_decodes_as_a_release_and_any_other_spelling_is_refused() {
    let row = |retires: &str, fixed_in: &str| {
        format!(
            "[[divergence]]\nid = \"trace:fixture\"\nkind = \"trace\"\ncause = \"php-ub\"\n{retires}fixed_in = \"{fixed_in}\"\nreason = \"r\"\nstatus = \"accepted\"\nsince = \"2026-11-10\"\n"
        )
    };
    let pin = "retires = \"pin\"\n";
    let parsed = Registry::parse(&row(pin, "8.5.13")).unwrap();
    let fixed: Release = "8.5.13".parse().unwrap();
    assert_eq!(parsed.rows()[0].fixed_in, Some(fixed));
    let written = toml::to_string(&parsed.rows()[0]).unwrap();
    assert!(
        written.contains("retires = \"pin\"\nfixed_in = \"8.5.13\"\n"),
        "{written}"
    );
    for spelt in [
        "8.5", "8.5.13.1", "8.6.0RC1", "8.5.x", "8..13", "v8.5.13", "",
    ] {
        let err = format!("{:#}", Registry::parse(&row(pin, spelt)).unwrap_err());
        assert!(
            err.contains("is not a php release spelt <major>.<minor>.<patch>"),
            "{spelt:?}: {err}"
        );
    }

    // The order is the numbers', not the text's.
    let release = |text: &str| text.parse::<Release>().unwrap();
    assert!(release("8.5.9") < release("8.5.12"));
    assert!(release("8.5.12") < release("8.6.0"));
    assert!(release("8.10.0") > release("8.9.99"));
    assert_eq!(release("8.5.12"), release("8.5.12"));
}

/// The manifest of [`reasoned`] with the tracked pin beside it, which a
/// row's `fixed_in` is held to.
fn reasoned_and_pinned() -> Manifest {
    let mut manifest = reasoned();
    let text = std::fs::read_to_string(workspace().join("oracle.toml")).unwrap();
    manifest.pin = Some(Pin::parse(&text).unwrap());
    manifest
}

/// The guard's refusals beyond a missing reason,
/// each with a fixture under the rule's own that must fail: an accepted
/// `by-design` row whose reason cites no design section or ADR, an
/// accepted `oracle-build` row with no `retires`, an unpinned trace dump
/// and an accepted row whose `fixed_in` the pin has reached. The row
/// beside each, in order, is not named, and the rule's text is printed.
#[test]
fn the_guard_refuses_an_uncited_by_design_row_an_unretired_oracle_build_row_an_unpinned_dump_and_a_reached_release()
 {
    let manifest = reasoned_and_pinned();
    for (file, number, named, says) in [
        (
            "by-design-uncited.toml",
            "uncited",
            "phpt:fixture-uncited",
            "cites no design section",
        ),
        (
            "oracle-build-unretired.toml",
            "unretired",
            "crosscheck:fixture-unretired",
            "carries no `retires`",
        ),
        (
            "trace-dump-unpinned.toml",
            "unpinned",
            "trace:dump/fixture-unpinned",
            "pins neither side's lines",
        ),
        (
            "fixed-in-reached.toml",
            "reached",
            "trace:fixture-reached (fixed in 8.5.0, the pin ",
            "whose `fixed_in` the pin has reached",
        ),
    ] {
        let path = fixture("divergence.reason").with_file_name(file);
        let registry = Registry::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let reading = divergence::reason(&registry, &manifest, workspace()).unwrap();
        assert_eq!(reading.status, xtask::reading::Status::Fail, "{file}");
        assert_eq!(reading.numbers[number], 1.into(), "{file}");
        assert!(
            reading.message.contains(named) && reading.message.contains(says),
            "{file}: {}",
            reading.message
        );
        assert!(
            !reading.message.contains("fixture-in-order"),
            "{file}: {}",
            reading.message
        );
        let rule = manifest.rule("divergence.reason").unwrap();
        assert!(reading.message.contains(&rule.text), "{}", reading.message);
    }

    // What the guard reads is the field and the citation, never the words
    // about retiring: an ADR or a section is a citation, and a `retires`
    // answers it whatever the reason says.
    let cited = r#"
[[divergence]]
id = "trace:fixture-adr"
kind = "trace"
cause = "by-design"
reason = "Per-engine state, as ADR-0005 rules."
status = "accepted"
since = "2026-10-09"

[[divergence]]
id = "trace:fixture-retired"
kind = "trace"
cause = "oracle-build"
retires = "platform"
reason = "Says nothing of when."
status = "accepted"
since = "2026-10-09"

[[divergence]]
id = "trace:fixture-open"
kind = "trace"
cause = "by-design"
reason = ""
status = "open"
since = "2026-10-09"
"#;
    let reading =
        divergence::reason(&Registry::parse(cited).unwrap(), &manifest, workspace()).unwrap();
    assert_eq!(reading.status, xtask::reading::Status::Pass, "{reading:?}");
    let uncited = cited.replace("as ADR-0005 rules", "as ADR-zero rules");
    let reading =
        divergence::reason(&Registry::parse(&uncited).unwrap(), &manifest, workspace()).unwrap();
    assert!(
        reading.message.contains("trace:fixture-adr"),
        "an ADR is its number: {}",
        reading.message
    );

    // A release the pin has reached is refused on an accepted row alone:
    // a fixed row keeps it as the record, and with no pin standing
    // nothing is reached.
    let reached = std::fs::read_to_string(
        fixture("divergence.reason").with_file_name("fixed-in-reached.toml"),
    )
    .unwrap();
    let fixed = reached.replacen("status = \"accepted\"", "status = \"fixed\"", 1);
    let reading =
        divergence::reason(&Registry::parse(&fixed).unwrap(), &manifest, workspace()).unwrap();
    assert_eq!(reading.status, xtask::reading::Status::Pass, "{reading:?}");
    let reading = divergence::reason(
        &Registry::parse(&reached).unwrap(),
        &reasoned(),
        workspace(),
    )
    .unwrap();
    assert_eq!(reading.status, xtask::reading::Status::Pass, "{reading:?}");
    assert_eq!(reading.numbers["reached"], 0.into());
}

/// A bump that lands exactly on the release a row names is the main case
/// of the ruling of 2026-11-10: a `fixed_in` equal to the pin's own
/// version, read from the pin at run time since no source states it, is
/// reached and refused, and the row beside it, ahead of the pin, is not.
#[test]
fn a_row_fixed_in_the_pins_own_release_is_reached() {
    let manifest = reasoned_and_pinned();
    let version = &manifest.pin.as_ref().unwrap().oracle.version;
    let path = fixture("divergence.reason").with_file_name("fixed-in-reached.toml");
    let text = std::fs::read_to_string(path).unwrap();
    assert_eq!(text.matches("fixed_in = \"8.5.0\"").count(), 1, "{text}");
    let at_the_pin = text.replace("fixed_in = \"8.5.0\"", &format!("fixed_in = \"{version}\""));
    let reading = divergence::reason(
        &Registry::parse(&at_the_pin).unwrap(),
        &manifest,
        workspace(),
    )
    .unwrap();
    assert_eq!(reading.status, xtask::reading::Status::Fail, "{reading:?}");
    assert_eq!(reading.numbers["reached"], 1.into());
    assert!(
        reading.message.contains(&format!(
            "trace:fixture-reached (fixed in {version}, the pin {version})"
        )),
        "{}",
        reading.message
    );
    assert!(
        !reading.message.contains("fixture-in-order"),
        "{}",
        reading.message
    );
}

#[test]
fn status_divergence_counts_a_proposed_row_as_open() {
    let root = scratch("open");
    let mut registry = Registry::load(&root).unwrap();
    registry
        .propose("phpt:Zend/tests/x.phpt", Kind::Runner)
        .unwrap();
    let md = xtask(&root, &["status", "--section", "divergence"]).ok();
    assert!(
        md.contains("| phpt:Zend/tests/x.phpt | runner | debt | open | "),
        "the facts list an open row: {md}"
    );
    assert!(md.contains("| runner | 1 | 0 | 0 | 1 |"), "{md}");
    assert!(md.contains(&counts_line(&registry, None)), "{md}");
    assert!(md.contains("| total | 1 | "), "{md}");
}

/// A registry of one accepted row whose reason holds for two tests it
/// covers and not for its own id, which names the cause (revision 62).
const COVERING: &str = r#"
[[divergence]]
id = "phpt:fixture-optimizer"
kind = "phpt"
reason = "The fixture's one reason (fixture)."
status = "accepted"
since = "2026-09-28"
covers = [
    "phpt:php-src/ext/fixture/tests/a.phpt",
    "phpt:php-src/ext/fixture/tests/opt/b.phpt",
]
"#;

/// The negative control harness §4.3 names: a `phpt:` id listed only in
/// `covers` is diverged under the accepted row, and the same registry
/// without the row reads it as `Differs`.
#[test]
fn an_id_listed_only_in_covers_is_diverged_and_differs_without_the_row() {
    use xtask::oracle::{Verdict, compare};
    let covered = "phpt:php-src/ext/fixture/tests/opt/b.phpt";
    let registry = Registry::parse(COVERING).unwrap();
    let row = registry
        .row(covered)
        .expect("a covered id answers with its row");
    assert_eq!(row.id.as_str(), "phpt:fixture-optimizer");
    match compare(&registry, covered, b"five\n", b"four\n") {
        Verdict::Diverged(row) => assert_eq!(row.reason, "The fixture's one reason (fixture)."),
        other => panic!("covered, so diverged: {other:?}"),
    }
    assert_eq!(compare(&registry, covered, b"4\n", b"4\n"), Verdict::Same);
    assert!(
        matches!(
            compare(
                &registry,
                "phpt:php-src/ext/fixture/tests/c.phpt",
                b"5",
                b"4"
            ),
            Verdict::Differs(_)
        ),
        "an id the row does not name is not held"
    );
    assert_eq!(
        registry
            .row("phpt:fixture-optimizer")
            .map(|r| r.covers.len()),
        Some(2),
        "the row's own id answers too"
    );

    let without = Registry::parse("").unwrap();
    assert!(
        matches!(
            compare(&without, covered, b"five\n", b"four\n"),
            Verdict::Differs(_)
        ),
        "without the row the difference is not accepted"
    );

    let open = Registry::parse(&COVERING.replace("\"accepted\"", "\"open\"")).unwrap();
    assert!(
        matches!(compare(&open, covered, b"5", b"4"), Verdict::Differs(_)),
        "an open covering row accepts nothing, as an open row does"
    );

    let counts = registry.counts();
    assert_eq!(registry.rows().len(), 1);
    assert_eq!(
        counts.debt(Kind::Phpt, Status::Accepted),
        1,
        "a covering row counts once"
    );
    assert_eq!(counts.ids, 3, "its own id and the two it covers");
}

/// An id in `covers` is held to its row's kind as the row's own id is, in
/// the same words; and an id is held by one row only, as its own or
/// covered, and named once within a row.
#[test]
fn a_covered_id_is_held_to_the_kind_and_to_one_row() {
    let rule = |err: &str| err.contains("is of kind phpt, whose ids start with \"phpt:\"");
    let own = Registry::parse(&COVERING.replace(
        "id = \"phpt:fixture-optimizer\"",
        "id = \"fixture-optimizer\"",
    ))
    .unwrap_err()
    .to_string();
    assert!(rule(&own), "{own}");
    let covered = Registry::parse(&COVERING.replace(
        "\"phpt:php-src/ext/fixture/tests/a.phpt\"",
        "\"php-src/ext/fixture/tests/a.phpt\"",
    ))
    .unwrap_err()
    .to_string();
    assert!(
        rule(&covered) && covered.contains("php-src/ext/fixture/tests/a.phpt"),
        "the rule's words, as a row's own id is refused: {covered}"
    );
    assert!(
        covered.contains("phpt:fixture-optimizer"),
        "the row is named: {covered}"
    );

    let twice = Registry::parse(&COVERING.replace(
        "\"phpt:php-src/ext/fixture/tests/opt/b.phpt\"",
        "\"phpt:php-src/ext/fixture/tests/a.phpt\"",
    ))
    .unwrap_err()
    .to_string();
    assert!(twice.contains("named twice"), "{twice}");
    let itself = Registry::parse(&COVERING.replace(
        "\"phpt:php-src/ext/fixture/tests/opt/b.phpt\"",
        "\"phpt:fixture-optimizer\"",
    ))
    .unwrap_err()
    .to_string();
    assert!(itself.contains("named twice"), "{itself}");

    let other = r#"
[[divergence]]
id = "phpt:php-src/ext/fixture/tests/a.phpt"
kind = "phpt"
reason = "Another reason."
status = "accepted"
since = "2026-09-28"
"#;
    for text in [format!("{COVERING}{other}"), format!("{other}{COVERING}")] {
        let err = Registry::parse(&text).unwrap_err().to_string();
        assert!(
            err.contains("phpt:php-src/ext/fixture/tests/a.phpt") && err.contains("two rows"),
            "an id covered by one row and another's own: {err}"
        );
    }
    let again = COVERING.replace("phpt:fixture-optimizer", "phpt:fixture-optimizer-2");
    let err = Registry::parse(&format!("{COVERING}{again}"))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("two rows") && err.contains("rows 1 and 2"),
        "an id two rows cover: {err}"
    );

    let root = scratch("propose-covered");
    std::fs::write(root.join(divergence::FILE), COVERING).unwrap();
    let mut registry = Registry::load(&root).unwrap();
    let err = registry
        .propose("phpt:php-src/ext/fixture/tests/a.phpt", Kind::Phpt)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("phpt:fixture-optimizer"),
        "a covered id already has its row: {err}"
    );
}

#[test]
fn status_and_the_reason_instrument_count_a_covering_row_once() {
    let root = scratch("covering");
    let path = root.join(divergence::FILE);
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{text}\n{COVERING}")).unwrap();
    let registry = Registry::load(&root).unwrap();
    let md = xtask(&root, &["status", "--section", "divergence", "--uncut"]).ok();
    assert!(
        md.contains("| phpt:fixture-optimizer (covers 2) | phpt | debt | accepted | 2026-09-28 |"),
        "the row once, with how many ids it covers:\n{md}"
    );
    assert!(
        !md.contains("tests/opt/b.phpt"),
        "a covered id is counted in the row, not listed as one:\n{md}"
    );
    assert!(
        md.contains(&counts_line(&registry, Some(Kind::Phpt))),
        "{md}"
    );
    assert!(md.contains(&counts_line(&registry, None)), "{md}");
    let json = xtask(
        &root,
        &["status", "--section", "divergence", "--format", "json"],
    )
    .ok();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let row = value["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "phpt:fixture-optimizer")
        .unwrap();
    assert_eq!(row["covers"].as_array().map(Vec::len), Some(2), "{row}");

    let manifest = reasoned();
    let unreasoned =
        Registry::parse(&COVERING.replace("The fixture's one reason (fixture).", "")).unwrap();
    let reading = divergence::reason(&unreasoned, &manifest, workspace()).unwrap();
    assert_eq!(reading.status, xtask::reading::Status::Fail);
    assert!(
        reading
            .message
            .contains("phpt:fixture-optimizer (covering 2 ids)"),
        "{}",
        reading.message
    );
    assert_eq!(reading.numbers["rows"], 1.into());
    assert_eq!(reading.numbers["ids"], 3.into());
    assert_eq!(reading.numbers["unreasoned"], 1.into());
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
    fn the_tracked_registry_loads_and_every_row_answers_for_itself() {
        // What holds of any registry a person keeps, and nothing about which
        // rows it keeps: a row dropped or added breaks no test here.
        let registry = Registry::load(workspace()).unwrap();
        for row in registry.rows() {
            let id = row.id.as_str();
            assert_eq!(registry.row(id).map(|r| &r.id), Some(&row.id), "{id}");
            assert_eq!(
                registry.accepted(id).is_some(),
                row.status == Status::Accepted,
                "{id}"
            );
            if row.status == Status::Accepted {
                assert!(!row.reason.is_empty(), "{id}: an accepted row has a reason");
            }
        }
        assert!(registry.accepted("trace:no-such").is_none());
        let counts = registry.counts();
        assert_eq!(
            counts.by_status.values().sum::<u64>(),
            u64::try_from(registry.rows().len()).unwrap()
        );
        assert_eq!(
            Cause::ALL
                .into_iter()
                .map(|c| counts.rows_of(c))
                .sum::<u64>(),
            u64::try_from(registry.rows().len()).unwrap(),
            "every row is of one cause"
        );
    }

    #[test]
    fn the_reason_instrument_passes_the_tracked_registry() {
        let manifest = Manifest::load(workspace()).unwrap();
        let registry = Registry::load(workspace()).unwrap();
        let reading = divergence::reason(&registry, &manifest, workspace()).unwrap();
        assert_eq!(reading.status, xtask::reading::Status::Pass, "{reading:?}");
        assert_eq!(reading.instrument, divergence::INSTRUMENT);
        assert_eq!(reading.rule, "divergence.reason");
        assert_eq!(
            reading.numbers["rows"],
            xtask::reading::Number::count(registry.rows().len())
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
            let reading = divergence::reason(&registry, &manifest, workspace()).unwrap();
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

    /// `docs/divergence.md` is the view; the registry is the truth. Its region's
    /// command is the status section with every reason whole, over the
    /// registry beside it, so its body is that rendering: the body opens with
    /// the section's own table head, as the section renders it over a registry
    /// of one row. Whether the body is the rendering of the rows the registry
    /// has now is `computed check`'s to hold, which pre-commit runs after
    /// `xtask regions` has rendered it and CI runs over the tree; held here, a
    /// merge that brings rows from both sides fails before the hook renders
    /// them.
    #[test]
    fn docs_divergence_md_is_the_status_section_as_a_computed_region() {
        let page = std::fs::read_to_string(workspace().join("docs/divergence.md")).unwrap();
        let opener = page
            .lines()
            .find(|l| l.starts_with("<!-- computed exec"))
            .expect("one exec region");
        assert!(
            opener.contains("cmd=\"cargo xtask status --section divergence --uncut --tracked\""),
            "the command is the status section, reasons whole: {opener}"
        );
        assert!(opener.contains("inputs=divergence.toml"), "{opener}");
        let start = page.find(opener).unwrap() + opener.len();
        let end = page[start..].find("<!-- /computed").unwrap() + start;
        let body = page[start..end].trim();

        let root = scratch("page");
        std::fs::write(root.join(divergence::FILE), COVERING).unwrap();
        let md = xtask(&root, &["status", "--section", "divergence", "--uncut"]).ok();
        let head: Vec<&str> = md.trim().lines().take(2).collect();
        assert_eq!(head.len(), 2, "{md}");
        assert_eq!(
            body.lines().take(2).collect::<Vec<_>>(),
            head,
            "the body is the section's rendering"
        );
    }
}
