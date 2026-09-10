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
    status_with_stdin(root, args, "")
}

/// `status` with the hook's stdin, the JSON head every hook event carries.
fn status_with_stdin(root: &Path, args: &[&str], stdin: &str) -> (bool, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("--root")
        .arg(root)
        .arg("status")
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), stdin.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
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
    store.record(std::slice::from_ref(&api)).unwrap();

    let mut heap = Reading::new("budget", "php_heap");
    heap.status = Status::Fail;
    heap.numbers.insert("lines".into(), 20001.into());
    heap.message = "php_heap: 20001 lines over the ceiling of 20000".into();
    store.record(std::slice::from_ref(&heap)).unwrap();

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
    let (ok, _, err) = status(&root, &["--section", "gates"]);
    assert!(!ok);
    assert!(err.contains("gates"), "{err}");
}

/// The volatile sections of the local file (harness-design.md §5) render
/// the readings of an instrument that does not exist yet as "no reading",
/// and one that was written as its row.
#[test]
fn the_local_sections_render_from_the_store_alone() {
    let root = scratch("local");
    for section in ["phpt", "witness", "mutants", "permits", "bundle"] {
        let (ok, md, err) = status(&root, &["--section", section]);
        assert!(ok, "{section}: {err}");
        assert!(md.contains("no reading"), "{section}: {md}");
    }

    let store = Store::of_workspace(&root);
    let mut phpt = Reading::new("phpt", "Zend/tests");
    phpt.status = Status::Fail;
    phpt.numbers.insert("passed".into(), 9.into());
    phpt.numbers.insert("failed".into(), 1.into());
    phpt.message = "Zend/tests: 1 of 10 failed".into();
    store.record(std::slice::from_ref(&phpt)).unwrap();
    let (ok, md, _) = status(&root, &["--section", "phpt"]);
    assert!(ok);
    assert!(
        md.contains("Zend/tests") && md.contains("failed=1") && md.contains("fail"),
        "{md}"
    );
    assert!(!md.contains("no reading"), "{md}");

    let mut denial = Reading::new("permit", "harness.self");
    denial.rule = "harness.self".into();
    denial.status = Status::Fail;
    denial.adr = String::new();
    denial.message = "Edit seams.toml: Harness configuration ... by a person.".into();
    store.record(std::slice::from_ref(&denial)).unwrap();
    store.record(std::slice::from_ref(&denial)).unwrap();
    let (ok, md, _) = status(&root, &["--section", "permits"]);
    assert!(ok, "{md}");
    let row = md.lines().find(|l| l.contains("harness.self")).unwrap();
    assert!(row.contains("| 2 |"), "two denials in the log: {row}");
    assert!(
        md.contains("ext.no-locks") && md.contains("| 0 |"),
        "a rule that never fired says so: {md}"
    );

    let (ok, json, _) = status(&root, &["--section", "permits", "--format", "json"]);
    assert!(ok);
    let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
    let row = rows["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["rule"] == "harness.self")
        .unwrap();
    assert_eq!(row["denials"], 2);
    assert_eq!(row["reading"]["status"], "fail");
}

/// `git` is volatile: the log and the status of the working tree.
#[test]
fn git_prints_the_log_and_the_status() {
    let root = scratch("git");
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["init", "-q", "-b", "main"]);
    run(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "the first commit",
    ]);
    std::fs::write(root.join("untracked.txt"), "x").unwrap();
    let (ok, md, err) = status(&root, &["--section", "git"]);
    assert!(ok, "{err}");
    assert!(md.contains("the first commit"), "{md}");
    assert!(md.contains("?? untracked.txt"), "{md}");
}

/// `--session` is the map plus the local facts in one block, for the
/// session-start hook; the local file is laid down from its template when
/// the clone has none, and an unrendered region is said to be one.
#[test]
fn session_prints_the_map_and_the_local_facts_in_one_block() {
    let root = scratch("session");
    std::fs::write(
        root.join("AGENTS.md"),
        "# The map

Read CONTEXT.md first.
",
    )
    .unwrap();
    let local = root.join(".claude/harness.local.md");
    assert!(!local.exists());

    let (ok, text, err) = status(&root, &["--session"]);
    assert!(ok, "{err}");
    assert!(
        text.starts_with(
            "# The map
"
        ),
        "{text}"
    );
    assert!(
        local.exists(),
        "the local file is laid down from its template"
    );
    let template = std::fs::read_to_string(&local).unwrap();
    for name in [
        "budget",
        "phpt",
        "witness",
        "mutants",
        "divergence",
        "permits",
        "git",
    ] {
        assert!(
            template.contains(&format!("--section {name}\" volatile name={name}")),
            "{name} is a volatile region of the local file: {template}"
        );
    }
    assert!(text.contains("unrendered"), "{text}");
    assert!(text.contains("computed trust"), "{text}");

    std::fs::write(
        &local,
        "# Local\n\n<!-- computed exec cmd=\"x\" volatile name=budget -->\n| crate |\n<!-- /computed in=a out=b -->\n",
    )
    .unwrap();
    let (ok, text, _) = status(&root, &["--session"]);
    assert!(ok);
    assert!(
        text.contains(
            "# The map
"
        ) && text.contains("| crate |"),
        "{text}"
    );
    assert!(!text.contains("unrendered"), "{text}");

    let (ok, _, err) = status(&root, &["--session", "--section", "rules"]);
    assert!(!ok, "one of --section and --session: {err}");
    let (ok, _, _) = status(&root, &[]);
    assert!(!ok, "one of --section and --session is required");
}

/// Three denials of one rule across two sessions: the rule's row counts
/// them per session, newest session first, and in total (§10 rule 6).
#[test]
fn permits_counts_denials_per_session_and_in_total() {
    let root = scratch("sessions");
    let store = Store::of_workspace(&root);
    let mut denial = Reading::new("permit", "harness.self");
    denial.rule = "harness.self".into();
    denial.status = Status::Fail;
    denial.message = "Edit seams.toml: edited by a person.".into();
    denial.session = "3f1c0d2a-9b7e-4c55-8f1a-2d6e7b9c0a11".into();
    store.record(std::slice::from_ref(&denial)).unwrap();
    store.record(std::slice::from_ref(&denial)).unwrap();
    denial.session = "9a8b7c6d-0000-4000-8000-000000000000".into();
    store.record(std::slice::from_ref(&denial)).unwrap();

    let (ok, md, err) = status(&root, &["--section", "permits"]);
    assert!(ok, "{err}");
    let row = md.lines().find(|l| l.contains("harness.self")).unwrap();
    assert!(row.contains("| 3 |"), "three denials in total: {row}");
    assert!(
        row.contains("9a8b7c6d=1 3f1c0d2a=2"),
        "per session, the newest first: {row}"
    );

    let (ok, json, _) = status(&root, &["--section", "permits", "--format", "json"]);
    assert!(ok);
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let row = value["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["rule"] == "harness.self")
        .unwrap();
    assert_eq!(row["denials"], 3);
    assert_eq!(
        row["sessions"],
        serde_json::json!([
            {"session": "9a8b7c6d-0000-4000-8000-000000000000", "denials": 1},
            {"session": "3f1c0d2a-9b7e-4c55-8f1a-2d6e7b9c0a11", "denials": 2},
        ])
    );
}

/// permit judges by two rules of its own beside the manifest's, the
/// region body and its own build, and the section carries a row for each:
/// through the outage of #16 nothing under `permits` said the judge was
/// down, and a rule with no row is a rule nothing renders.
#[test]
fn permits_carries_a_row_for_the_rules_permit_judges_by_that_are_its_own() {
    let root = scratch("permit-own-rules");
    let store = Store::of_workspace(&root);
    let mut refusal = Reading::new("permit", "permit.stale-build");
    refusal.rule = "permit.stale-build".into();
    refusal.status = Status::Fail;
    refusal.message = "Edit seams.toml: this call was not judged.".into();
    refusal.session = "3f1c0d2a-9b7e-4c55-8f1a-2d6e7b9c0a11".into();
    store.record(std::slice::from_ref(&refusal)).unwrap();

    let (ok, md, err) = status(&root, &["--section", "permits"]);
    assert!(ok, "{err}");
    let row = md
        .lines()
        .find(|l| l.contains("permit.stale-build"))
        .unwrap_or_else(|| panic!("no row for the refusal:\n{md}"));
    assert!(row.contains("| 1 |"), "the refusal is counted: {row}");
    assert!(row.contains("not judged"), "and says so: {row}");
    assert!(
        md.contains("computed.region-body"),
        "permit's other rule of its own has a row too:\n{md}"
    );
}

/// The row #33 proposes, `git.pathspec`, appended to the scratch
/// manifest verbatim: the file parses with it, and `permits` carries the
/// rule's row, so a denial is visible where the facts are. Once the
/// tracked manifest carries the row this proves the same of it.
#[test]
fn permits_carries_a_row_for_the_proposed_git_pathspec_rule() {
    let root = scratch("permit-pathspec-row");
    let manifest = root.join("seams.toml");
    let mut text = std::fs::read_to_string(&manifest).unwrap();
    if !text.contains("id = \"git.pathspec\"") {
        text.push_str(
            "\n[[rule]]\nid = \"git.pathspec\"\ntool = \"Bash\"\nwith = [\"git add\", \"git commit\"]\nrequire_argv = \"--\"\ntext = \"An agent stages and commits by pathspec, `git add -- <paths>` and `git commit -F <file> -- <paths>`: the index is shared with every session in the checkout.\"\n",
        );
        std::fs::write(&manifest, text).unwrap();
    }
    let store = Store::of_workspace(&root);
    let mut denial = Reading::new("permit", "git add -A && git commit -m x");
    denial.rule = "git.pathspec".into();
    denial.status = Status::Fail;
    denial.message = "An agent stages and commits by pathspec".into();
    denial.session = "3f1c0d2a-9b7e-4c55-8f1a-2d6e7b9c0a11".into();
    store.record(std::slice::from_ref(&denial)).unwrap();

    let (ok, md, err) = status(&root, &["--section", "permits"]);
    assert!(ok, "{err}");
    let row = md
        .lines()
        .find(|l| l.contains("git.pathspec"))
        .unwrap_or_else(|| panic!("no row for the rule:\n{md}"));
    assert!(row.contains("| 1 |"), "the denial is counted: {row}");
}

/// The bench's reading is a measurement whose exit is not its status
/// (harness-design.md §4.4, #61), and the section renders it as before:
/// a `fail` against the bar is one reading and one failure of `permit`
/// in the instruments table, and no rule's row, since it judged none.
#[test]
fn permits_renders_the_bench_reading_as_before() {
    let root = scratch("bench-row");
    let store = Store::of_workspace(&root);
    let mut bench = Reading::new("permit", "xtask");
    bench.status = Status::Fail;
    bench.numbers.insert("p99_ms".into(), 67.0.into());
    bench.numbers.insert("bar_ms".into(), 100.0.into());
    bench.message = "permit at the tool call: p99 67.0 ms against the bar of 100 ms".into();
    store
        .record_measurement(std::slice::from_ref(&bench))
        .unwrap();

    let (ok, md, err) = status(&root, &["--section", "permits"]);
    assert!(ok, "{err}");
    let row = md
        .lines()
        .find(|l| l.starts_with("| permit |"))
        .unwrap_or_else(|| panic!("no line for permit:\n{md}"));
    assert!(row.contains("| 1 | 1 |"), "one reading, one failure: {row}");
    let (ok, json, _) = status(&root, &["--section", "permits", "--format", "json"]);
    assert!(ok);
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let permit = value["instruments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["instrument"] == "permit")
        .unwrap()
        .clone();
    assert_eq!(permit["readings"], 1);
    assert_eq!(permit["failures"], 1);
    assert!(
        value["rules"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["denials"] == 0),
        "a missed bar is no denial: {json}"
    );
}

/// The query behind CI's §7 section (harness-design.md): readings and
/// failures per instrument from the log, and an instrument with readings
/// but no failure in fourteen days is a candidate for silent; one whose
/// fixture failed within the window is not.
#[test]
fn permits_lists_the_instruments_and_the_candidates_for_silent() {
    let root = scratch("silent");
    let store = Store::of_workspace(&root);
    // The tracked manifest carries the window; the first rendering is
    // without it, so strip the table and its one key.
    let manifest_path = root.join("seams.toml");
    let mut manifest = String::new();
    for line in std::fs::read_to_string(&manifest_path).unwrap().lines() {
        if !line.starts_with("[telemetry]") && !line.starts_with("silent_days") {
            manifest.push_str(line);
            manifest.push('\n');
        }
    }
    std::fs::write(&manifest_path, &manifest).unwrap();

    let mut budget = Reading::new("budget", "php_heap");
    budget.status = Status::Pass;
    store.record(std::slice::from_ref(&budget)).unwrap();
    store.record(std::slice::from_ref(&budget)).unwrap();

    let mut vocab = Reading::new("vocab", "php_heap");
    vocab.status = Status::Pass;
    store.record(std::slice::from_ref(&vocab)).unwrap();
    vocab.scope = "fixture".into();
    vocab.status = Status::Fail;
    vocab.at = xtask::reading::now().unwrap();
    store.record(std::slice::from_ref(&vocab)).unwrap();

    let mut old = Reading::new("clippy", "php_heap");
    old.status = Status::Fail;
    store.record(std::slice::from_ref(&old)).unwrap();

    let (ok, md, err) = status(&root, &["--section", "permits"]);
    assert!(ok, "{err}");
    assert!(
        !md.contains(" silent |") && md.contains("silent_days"),
        "no window in the manifest: no silent column, and the key is named: {md}"
    );
    let (ok, json, _) = status(&root, &["--section", "permits", "--format", "json"]);
    assert!(ok);
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["instruments"][0]["silent"], serde_json::Value::Null);

    std::fs::write(
        &manifest_path,
        format!("{manifest}\n[telemetry]\nsilent_days = 14\n"),
    )
    .unwrap();
    let (ok, md, err) = status(&root, &["--section", "permits"]);
    assert!(ok, "{err}");
    let row = |name: &str| {
        md.lines()
            .find(|l| l.starts_with(&format!("| {name} |")))
            .unwrap_or_else(|| panic!("{name}: {md}"))
            .to_owned()
    };
    assert!(row("budget").contains("| 2 | 0 |"), "{}", row("budget"));
    assert!(row("budget").contains("candidate"), "{}", row("budget"));
    assert!(row("vocab").contains("| 2 | 1 |"), "{}", row("vocab"));
    assert!(!row("vocab").contains("candidate"), "{}", row("vocab"));
    assert!(
        row("clippy").contains("candidate"),
        "a failure at the epoch is not one in the window: {}",
        row("clippy")
    );

    let (ok, json, _) = status(&root, &["--section", "permits", "--format", "json"]);
    assert!(ok);
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let instruments = value["instruments"].as_array().unwrap();
    let of = |name: &str| {
        instruments
            .iter()
            .find(|r| r["instrument"] == name)
            .unwrap()
            .clone()
    };
    assert_eq!(of("budget")["readings"], 2);
    assert_eq!(of("budget")["failures"], 0);
    assert_eq!(of("budget")["silent"], true);
    assert_eq!(of("vocab")["failures"], 1);
    assert_eq!(of("vocab")["silent"], false);
}

/// The weekly exit bar table (harness-design.md §9): one row per `[[week]]`
/// of the manifest, pass when every reading it names holds, fail when one
/// contradicts, "no reading" while one is missing; `--week N` is one row.
#[test]
fn week_renders_one_row_per_bar_from_the_readings() {
    let root = scratch("week");
    let mut manifest = std::fs::read_to_string(root.join("seams.toml")).unwrap();
    manifest.push_str(
        r#"
[[week]]
days = "1-2"
text = "every instrument writes a reading"
reads = [{ instrument = "budget", scope = "xtask" }]
[[week]]
days = "3-7"
text = "xtask under 7000 lines, api within 400 items"
reads = [
  { instrument = "budget", scope = "xtask", number = "lines", max = 7000 },
  { instrument = "api-check", scope = "api", number = "items", max = 400 },
]
[[week]]
days = "8-14"
text = "vocab clean on xtask"
reads = [{ instrument = "vocab", scope = "xtask", number = "avoided", max = 0 }]
"#,
    );
    std::fs::write(root.join("seams.toml"), manifest).unwrap();

    let store = Store::of_workspace(&root);
    let mut budget = Reading::new("budget", "xtask");
    budget.status = Status::Pass;
    store.record(std::slice::from_ref(&budget)).unwrap();
    let mut lines = Reading::new("budget", "xtask");
    lines.status = Status::Pass;
    lines.numbers.insert("lines".into(), 6508.into());
    store.record(std::slice::from_ref(&lines)).unwrap();
    let mut items = Reading::new("api-check", "api");
    items.status = Status::Fail;
    items.numbers.insert("items".into(), 412.5.into());
    store.record(std::slice::from_ref(&items)).unwrap();
    let mut crashed = Reading::new("vocab", "xtask");
    crashed.status = Status::Fail;
    crashed.numbers.insert("avoided".into(), 0.into());

    let (ok, md, err) = status(&root, &["--section", "week"]);
    assert!(ok, "{err}");
    let rows: Vec<&str> = md.lines().skip(2).collect();
    assert_eq!(rows.len(), 3, "{md}");
    assert!(
        rows[0].contains("| 1-2 |") && rows[0].contains("| pass |"),
        "{}",
        rows[0]
    );
    assert!(
        rows[1].contains("| 3-7 |") && rows[1].contains("| fail |"),
        "{}",
        rows[1]
    );
    assert!(
        rows[1].contains("api items=412.5 > 400"),
        "the reading that missed the bar is named: {}",
        rows[1]
    );
    assert!(
        rows[2].contains("| 8-14 |") && rows[2].contains("| no reading |"),
        "{}",
        rows[2]
    );
    store.record(std::slice::from_ref(&crashed)).unwrap();
    let (ok, md, _) = status(&root, &["--section", "week", "--week", "3"]);
    assert!(ok);
    assert!(
        md.contains("| fail |"),
        "a number inside the bound on a reading that failed is no pass: {md}"
    );

    let (ok, md, err) = status(&root, &["--section", "week", "--week", "2"]);
    assert!(ok, "{err}");
    assert_eq!(md.lines().skip(2).count(), 1, "{md}");
    assert!(md.contains("| 3-7 |"), "{md}");
    let (ok, _, err) = status(&root, &["--section", "week", "--week", "4"]);
    assert!(!ok, "a week the manifest has no row for: {err}");
    let (ok, _, err) = status(&root, &["--section", "rules", "--week", "1"]);
    assert!(!ok, "--week belongs to the week section: {err}");

    let (ok, json, _) = status(&root, &["--section", "week", "--format", "json"]);
    assert!(ok);
    let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows[0]["days"], "1-2");
    assert_eq!(rows[0]["verdict"], "pass");
    assert_eq!(rows[1]["verdict"], "fail");
    assert_eq!(rows[1]["reads"][1]["verdict"], "fail");
    assert_eq!(rows[1]["reads"][1]["value"], 412.5);
    assert_eq!(rows[2]["verdict"], "fail", "the crashed vocab run");
    assert_eq!(rows[2]["reads"][0]["status"], "fail");
}

/// "This session" (harness-design.md §4.6): `--session-id` names it, and
/// the rule's row shows its count apart; `--session` takes the id from
/// the hook's stdin head and appends this session's denials after the
/// local facts, nothing when the session has none.
#[test]
fn permits_marks_this_session_and_the_session_block_carries_it() {
    let root = scratch("this-session");
    std::fs::write(root.join("AGENTS.md"), "# The map\n").unwrap();
    let store = Store::of_workspace(&root);
    let mut denial = Reading::new("permit", "harness.self");
    denial.rule = "harness.self".into();
    denial.status = Status::Fail;
    denial.message = "Edit seams.toml: edited by a person.".into();
    denial.session = "3f1c0d2a-9b7e-4c55-8f1a-2d6e7b9c0a11".into();
    store.record(std::slice::from_ref(&denial)).unwrap();
    denial.session = "9a8b7c6d-0000-4000-8000-000000000000".into();
    store.record(std::slice::from_ref(&denial)).unwrap();
    store.record(std::slice::from_ref(&denial)).unwrap();

    let this = "3f1c0d2a-9b7e-4c55-8f1a-2d6e7b9c0a11";
    let (ok, md, err) = status(&root, &["--section", "permits", "--session-id", this]);
    assert!(ok, "{err}");
    let row = md.lines().find(|l| l.contains("harness.self")).unwrap();
    assert!(row.contains("| 3 | 1 |"), "total, then this session: {row}");
    let row = md.lines().find(|l| l.contains("ext.no-locks")).unwrap();
    assert!(row.contains("| 0 | 0 |"), "{row}");
    let (_, md, _) = status(&root, &["--section", "permits"]);
    let row = md.lines().find(|l| l.contains("harness.self")).unwrap();
    assert!(
        row.contains("| 3 |  |"),
        "no session named, no count: {row}"
    );

    let (ok, json, _) = status(
        &root,
        &[
            "--section",
            "permits",
            "--session-id",
            this,
            "--format",
            "json",
        ],
    );
    assert!(ok);
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let row = value["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["rule"] == "harness.self")
        .unwrap();
    assert_eq!(row["this_session"], 1);
    assert_eq!(value["session"], this);

    let head = format!(
        r#"{{"session_id":"{this}","transcript_path":"x","cwd":"x","hook_event_name":"SessionStart","source":"resume"}}"#
    );
    let (ok, text, err) = status_with_stdin(&root, &["--session"], &head);
    assert!(ok, "{err}");
    assert!(text.starts_with("# The map\n"), "{text}");
    assert!(text.contains("## permits this session"), "{text}");
    let row = text.lines().find(|l| l.contains("harness.self")).unwrap();
    assert!(row.contains("| 3 | 1 |"), "{row}");
    assert!(
        !text.lines().any(|l| l.contains("ext.no-locks")),
        "only the rules this session fired: {text}"
    );

    let quiet = r#"{"session_id":"00000000-0000-4000-8000-000000000000","hook_event_name":"SessionStart","source":"startup"}"#;
    let (ok, text, _) = status_with_stdin(&root, &["--session"], quiet);
    assert!(ok);
    assert!(
        !text.contains("this session"),
        "a session with no denial adds nothing: {text}"
    );
    let (ok, text, _) = status_with_stdin(&root, &["--session"], "not json");
    assert!(ok, "a head that is not JSON is no session, not an error");
    assert!(!text.contains("this session"), "{text}");
}

/// A torn line in the log (#56) degrades the `permits` section rather than
/// the session: the section renders the readings around it and names the
/// line, and `--session` prints the map and the local facts with the note
/// under this session's block, where before both failed on the line.
#[test]
fn a_torn_log_line_is_named_and_the_rest_of_the_section_and_the_session_render() {
    let root = scratch("torn");
    std::fs::write(root.join("AGENTS.md"), "# The map\n").unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/store/torn.jsonl");
    let store = Store::of_workspace(&root);
    std::fs::create_dir_all(store.root()).unwrap();
    std::fs::copy(&fixture, store.root().join("log.jsonl")).unwrap();

    let (ok, md, err) = status(&root, &["--section", "permits"]);
    assert!(ok, "{err}");
    let row = md.lines().find(|l| l.contains("harness.self")).unwrap();
    assert!(
        row.contains("| 1 |"),
        "the readable denial is counted: {row}"
    );
    let row = md.lines().find(|l| l.contains("lanes.nightly")).unwrap();
    assert!(row.contains("| 1 |"), "and the one after the tear: {row}");
    assert!(
        md.contains("log.jsonl: 2 lines unreadable at 2, 3."),
        "the torn lines are named: {md}"
    );

    let (ok, json, _) = status(&root, &["--section", "permits", "--format", "json"]);
    assert!(ok);
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["unreadable"], serde_json::json!([2, 3]));

    let this = "5e55100c-0000-4000-8000-000000000000";
    let head = format!(r#"{{"session_id":"{this}","hook_event_name":"SessionStart"}}"#);
    let (ok, text, err) = status_with_stdin(&root, &["--session"], &head);
    assert!(ok, "{err}");
    assert!(text.starts_with("# The map\n"), "{text}");
    assert!(text.contains("## permits this session"), "{text}");
    assert!(
        text.contains("log.jsonl: 2 lines unreadable at 2, 3."),
        "{text}"
    );
}
