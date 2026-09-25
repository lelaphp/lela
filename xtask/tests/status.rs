//! `xtask status`, the one renderer (harness-design.md §4.6): it reads the
//! manifest and the store and computes nothing else.

use std::path::Path;
use std::process::Command;

use xtask::manifest::Manifest;
use xtask::oracle::{PROMISE_VARIABLE, TREE_VARIABLE};
use xtask::reading::{Reading, Status, Store};
use xtask::scratch::Tree;

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

/// A scratch workspace: the tracked manifest, pin and registry, and no store.
fn scratch(name: &str) -> Tree {
    let dir = Tree::new(&format!("status-{name}")).unwrap();
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

    // The ceiling column is the tracked manifest's `[budget] api_items`,
    // whatever a person has it at (#338).
    let ceiling = Manifest::load(&root).unwrap().budget.api_items.to_string();
    let (ok, md, _) = status(&root, &["--section", "locks"]);
    assert!(ok);
    assert!(
        md.contains("api.lock") && md.contains("301") && md.contains(&ceiling),
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
    assert!(
        md.lines().any(|l| l.starts_with("| scripts |")),
        "scripts/** has its row beside the crates (#34): {md}"
    );

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

/// Rows whose reading says the crate is not yet created fold into one row
/// at the end of the section, "N crates not yet created", naming them
/// (#64): twenty such rows carry one fact, and the session block has a
/// cap. A lone one stays as it is, and a crate that is measured keeps its
/// row; the fold reads the message alone, as budget, witness and phpt
/// write it.
#[test]
fn the_rows_of_crates_not_yet_created_fold_into_one() {
    let root = scratch("uncreated");
    let store = Store::of_workspace(&root);
    for name in ["php_heap", "php_unit", "ext/pcre"] {
        let mut reading = Reading::new("budget", name);
        reading.message = format!("{name}: not yet created");
        store.record(std::slice::from_ref(&reading)).unwrap();
    }
    let mut xtask = Reading::new("budget", "xtask");
    xtask.status = Status::Pass;
    xtask.numbers.insert("lines".into(), 11298.into());
    xtask.message = "xtask: 11298 lines under the ceiling of 14000".into();
    store.record(std::slice::from_ref(&xtask)).unwrap();

    let (ok, md, err) = status(&root, &["--section", "budget"]);
    assert!(ok, "{err}");
    let folded = md
        .lines()
        .find(|l| l.starts_with("| 3 crates not yet created |"))
        .unwrap_or_else(|| panic!("{md}"));
    assert!(
        folded.contains("| skip | not yet created: php_heap, php_unit, ext/pcre |"),
        "{folded}"
    );
    assert_eq!(md.lines().last(), Some(folded), "the fold is the last row");
    assert!(
        !md.lines().any(|l| l.starts_with("| php_heap |")),
        "a folded crate has no row of its own: {md}"
    );
    assert!(md.contains("| xtask | 11298 |"), "{md}");
    assert!(
        md.contains("| php_syntax |  | 26000 | no reading |"),
        "{md}"
    );

    let (ok, json, _) = status(&root, &["--section", "budget", "--format", "json"]);
    assert!(ok);
    let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
    let row = rows.as_array().unwrap().last().unwrap();
    assert_eq!(row["crate"], "3 crates not yet created");
    assert_eq!(row["reading"]["numbers"]["crates"], 3);

    let mut phpt = Reading::new("phpt", "ext/pcre");
    phpt.message = "ext/pcre: not yet created; the subset holds 165 tests".into();
    store.record(std::slice::from_ref(&phpt)).unwrap();
    let mut control = Reading::new("phpt", "ext/pcre@control");
    control.message = "ext/pcre@control: 0 of 165 differ".into();
    store.record(std::slice::from_ref(&control)).unwrap();
    let (ok, md, _) = status(&root, &["--section", "phpt"]);
    assert!(ok);
    assert!(
        md.lines().any(|l| l.starts_with("| ext/pcre | skip |")),
        "a lone row stays: {md}"
    );
    assert!(md.contains("| ext/pcre@control |"), "{md}");
    assert!(!md.contains("crates not yet created"), "{md}");
}

/// `status` with the two variables of the pin as the test wants them,
/// set or removed, since the process running the tests may carry either
/// (CI sets both for the suite).
fn status_with_pin(
    root: &Path,
    args: &[&str],
    tree: Option<&Path>,
    promise: Option<&str>,
) -> (bool, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_xtask"));
    command.arg("--root").arg(root).arg("status").args(args);
    match tree {
        Some(tree) => command.env(TREE_VARIABLE, tree),
        None => command.env_remove(TREE_VARIABLE),
    };
    match promise {
        Some(promise) => command.env(PROMISE_VARIABLE, promise),
        None => command.env_remove(PROMISE_VARIABLE),
    };
    let out = command.output().unwrap();
    (
        out.status.success(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

/// A scratch workspace whose pin's `[source].path` is `path`: the tracked
/// `oracle.toml` with that one line rewritten in the copy, the tracked
/// file untouched.
fn scratch_with_pin_path(name: &str, path: &Path) -> Tree {
    let root = scratch(name);
    let pin = root.join("oracle.toml");
    let mut in_source = false;
    let text: Vec<String> = std::fs::read_to_string(&pin)
        .unwrap()
        .lines()
        .map(|line| {
            if line.starts_with('[') {
                in_source = line.trim() == "[source]";
            }
            if in_source && line.starts_with("path = ") {
                format!("path = {:?}", path.display().to_string())
            } else {
                line.to_owned()
            }
        })
        .collect();
    std::fs::write(&pin, text.join("\n") + "\n").unwrap();
    root
}

/// A directory standing where the pinned php-src would.
fn fixture_tree(name: &str) -> Tree {
    Tree::new(&format!("status-oracle-{name}")).unwrap()
}

/// The `oracle` section as JSON, under the two variables as given.
fn oracle_json(root: &Path, tree: Option<&Path>, promise: Option<&str>) -> serde_json::Value {
    let (ok, json, err) = status_with_pin(
        root,
        &["--section", "oracle", "--format", "json"],
        tree,
        promise,
    );
    assert!(ok, "{err}");
    serde_json::from_str(&json).unwrap()
}

/// The `oracle` section (#36): the pin as this machine has it, the
/// variables by name and whether set, the pin's path, and where the tree
/// was found. Here the variable names a tree: found there, ahead of the
/// pin's path, which is a directory too.
#[test]
fn oracle_finds_the_tree_the_variable_names_and_states_both_names() {
    let named = fixture_tree("named");
    let pinned = fixture_tree("named-pinned");
    let root = scratch_with_pin_path("oracle-named", &pinned);
    let (ok, md, err) =
        status_with_pin(&root, &["--section", "oracle"], Some(&named), Some("tree"));
    assert!(ok, "{err}");
    let named_text = named.display().to_string();
    assert!(
        md.contains(&format!("| tree | {TREE_VARIABLE} | {named_text} | set |")),
        "{md}"
    );
    assert!(
        md.contains(&format!("| promise | {PROMISE_VARIABLE} | tree | set |")),
        "{md}"
    );
    assert!(
        md.contains(&format!(
            "| found |  | {named_text} | named by {TREE_VARIABLE} |"
        )),
        "{md}"
    );
    let json = oracle_json(&root, Some(&named), Some("tree"));
    assert_eq!(json["tree"]["variable"], TREE_VARIABLE);
    assert_eq!(json["tree"]["set"], true);
    assert_eq!(json["tree"]["value"], named_text);
    assert_eq!(json["promise"]["variable"], PROMISE_VARIABLE);
    assert_eq!(json["promise"]["set"], true);
    assert_eq!(json["promise"]["value"], "tree");
    assert_eq!(json["pin"]["path"], pinned.display().to_string());
    assert_eq!(json["pin"]["directory"], true);
    assert_eq!(json["found"]["at"], "variable");
    assert_eq!(json["found"]["path"], named_text);
    assert_eq!(json["found"]["error"], serde_json::Value::Null);
}

/// With the variable unset, the pin's path is the tree, and both rows
/// say unset.
#[test]
fn oracle_falls_back_to_the_pins_path_when_the_variable_is_unset() {
    let pinned = fixture_tree("pinned");
    let root = scratch_with_pin_path("oracle-pinned", &pinned);
    let (ok, md, err) = status_with_pin(&root, &["--section", "oracle"], None, None);
    assert!(ok, "{err}");
    assert!(
        md.contains(&format!("| tree | {TREE_VARIABLE} |  | unset |")),
        "{md}"
    );
    assert!(
        md.contains(&format!("| promise | {PROMISE_VARIABLE} |  | unset |")),
        "{md}"
    );
    assert!(
        md.contains(&format!("| pin |  | {} | a directory |", pinned.display())),
        "{md}"
    );
    assert!(
        md.contains(&format!(
            "| found |  | {} | the pin's path |",
            pinned.display()
        )),
        "{md}"
    );
    let json = oracle_json(&root, None, None);
    assert_eq!(json["tree"]["set"], false);
    assert_eq!(json["tree"]["value"], serde_json::Value::Null);
    assert_eq!(json["promise"]["set"], false);
    assert_eq!(json["promise"]["value"], serde_json::Value::Null);
    assert_eq!(json["found"]["at"], "pin");
    assert_eq!(json["found"]["path"], pinned.display().to_string());
}

/// With neither present, the found row names the variable, its state and
/// the pin's path, and the section renders rather than failing, writing
/// nothing.
#[test]
fn oracle_names_both_places_when_the_tree_is_at_neither() {
    let nowhere = Path::new("/nowhere/pinned/php-src");
    let root = scratch_with_pin_path("oracle-absent", nowhere);
    let (ok, md, err) = status_with_pin(&root, &["--section", "oracle"], None, None);
    assert!(ok, "{err}");
    assert!(
        md.contains(&format!(
            "| pin |  | {} | not a directory |",
            nowhere.display()
        )),
        "{md}"
    );
    let found = md.lines().find(|l| l.starts_with("| found |")).unwrap();
    assert!(found.contains("absent:"), "{found}");
    assert!(
        found.contains(&format!("{TREE_VARIABLE} is unset")),
        "{found}"
    );
    assert!(found.contains(&nowhere.display().to_string()), "{found}");
    let json = oracle_json(&root, None, None);
    assert_eq!(json["found"]["at"], serde_json::Value::Null);
    assert_eq!(json["found"]["path"], serde_json::Value::Null);
    let error = json["found"]["error"].as_str().unwrap();
    assert!(
        error.contains(TREE_VARIABLE) && error.contains("/nowhere/pinned/php-src"),
        "{error}"
    );
    assert!(
        !root.join("target").exists(),
        "status ran nothing and wrote nothing"
    );
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
    for section in ["phpt", "witness", "mutants", "ticket", "permits", "bundle"] {
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

    let mut ticket = Reading::new("ticket", "28");
    ticket.status = Status::Pass;
    ticket.numbers.insert("criteria".into(), 7.into());
    ticket.numbers.insert("ticked".into(), 7.into());
    ticket.numbers.insert("unticked".into(), 0.into());
    ticket.commit = "905018f".into();
    ticket.session = "session-abc".into();
    ticket.message = "#28: 7 of 7 criteria ticked; closed by xtask ticket close at 905018f".into();
    store.record(std::slice::from_ref(&ticket)).unwrap();
    let (ok, md, _) = status(&root, &["--section", "ticket"]);
    assert!(ok, "{md}");
    let row = md
        .lines()
        .find(|l| l.contains("| 28 |"))
        .unwrap_or_else(|| panic!("{md}"));
    assert!(
        row.contains("905018f") && row.contains("session-") && row.contains("ticked=7"),
        "what closed it and the session, from the store alone: {row}"
    );

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

/// A Markdown row is what a session scans at start, eight tables under
/// the engine's cap on one block (#820): its message is one line cut at
/// 120 characters and a number at zero is left out. What the row leaves
/// out is the same section's JSON, the reading whole.
#[test]
fn a_row_clips_its_message_and_leaves_zeros_out_and_the_json_has_them() {
    let root = scratch("clipped");
    let store = Store::of_workspace(&root);
    let mut phpt = Reading::new("phpt", "ext/dom");
    phpt.status = Status::Fail;
    phpt.numbers.insert("failed".into(), 691.into());
    phpt.numbers.insert("passed".into(), 0.into());
    phpt.numbers.insert("percent".into(), 0.0.into());
    let names = (0..40).map(|n| format!("t{n:03}.phpt")).collect::<Vec<_>>();
    phpt.message = format!("ext/dom: 691 of 871 missed:\n{}", names.join(", "));
    store.record(std::slice::from_ref(&phpt)).unwrap();

    let (ok, md, err) = status(&root, &["--section", "phpt"]);
    assert!(ok, "{err}");
    let row = md.lines().find(|l| l.starts_with("| ext/dom |")).unwrap();
    let cells: Vec<&str> = row.trim_matches('|').split(" | ").collect();
    assert_eq!(cells[2], "failed=691", "{row}");
    let message = cells[3].trim_end();
    assert_eq!(message.chars().count(), 120, "{message}");
    assert!(
        message.starts_with("ext/dom: 691 of 871 missed: t000.phpt"),
        "{message}"
    );
    assert!(
        message.ends_with('…') && !message.contains("t039"),
        "{message}"
    );

    let (ok, json, _) = status(&root, &["--section", "phpt", "--format", "json"]);
    assert!(ok);
    let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
    let reading = &rows[0]["reading"];
    assert_eq!(reading["message"], phpt.message.as_str());
    assert_eq!(reading["numbers"]["passed"], 0);
    assert_eq!(reading["numbers"]["percent"], 0.0);
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

    // As JSON, the two blocks the function hook hands the engine (#64):
    // the map as the file is, the facts with the note that they are
    // unrendered; the Markdown is the two joined.
    std::fs::write(&local, "# Local\n\n<!-- /computed -->\n").unwrap();
    let (ok, json, err) = status(&root, &["--session", "--format", "json"]);
    assert!(ok, "{err}");
    let blocks: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(blocks["map"], "# The map\n\nRead CONTEXT.md first.\n");
    let facts = blocks["facts"].as_str().unwrap();
    assert!(facts.starts_with("# Local\n"), "{facts}");
    assert!(facts.contains("unrendered"), "{facts}");
    let (_, text, _) = status(&root, &["--session"]);
    assert_eq!(
        text,
        format!("{}\n{facts}", blocks["map"].as_str().unwrap())
    );

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

/// permit judges by three rules of its own beside the manifest's, the
/// region body, a build apart from the tree and a call no candidate
/// judged, and the section carries a row for each: through the outage of
/// #16 nothing under `permits` said the judge was down, and a rule with no
/// row is a rule nothing renders.
///
/// The two refusals are counted in two columns (#118). A build that hands
/// the call to the candidate that rebuilds is a skip under `handed on`,
/// since the call was judged; a call that reached the end of the walk
/// unjudged is a failure under `denials`, since it was not. Counting both
/// as denials is what buried the 23 real ones under 5687 walks.
#[test]
fn permits_separates_the_calls_denied_from_the_calls_handed_to_the_next_candidate() {
    let root = scratch("permit-own-rules");
    let store = Store::of_workspace(&root);
    let session = "3f1c0d2a-9b7e-4c55-8f1a-2d6e7b9c0a11";

    let mut handed_on = Reading::new("permit", "permit.stale-build");
    handed_on.rule = "permit.stale-build".into();
    handed_on.status = Status::Skip;
    handed_on.message =
        "Edit seams.toml: this build did not judge this call and handed it to the next candidate."
            .into();
    handed_on.session = session.into();
    store.record(std::slice::from_ref(&handed_on)).unwrap();

    let mut unjudged = Reading::new("permit", "permit.unjudged-call");
    unjudged.rule = "permit.unjudged-call".into();
    unjudged.status = Status::Fail;
    unjudged.message = "Edit seams.toml: this call was not judged.".into();
    unjudged.session = session.into();
    store.record(std::slice::from_ref(&unjudged)).unwrap();

    let (ok, json, err) = status(&root, &["--section", "permits", "--format", "json"]);
    assert!(ok, "{err}");
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let row = |id: &str| {
        value["rules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["rule"] == id)
            .unwrap_or_else(|| panic!("no row for {id}:\n{json}"))
            .clone()
    };
    let stale = row("permit.stale-build");
    assert_eq!(stale["denials"], 0, "a walk is not a denial: {stale}");
    assert_eq!(stale["handed_on"], 1, "it is a hand-on: {stale}");
    let walked_past = row("permit.unjudged-call");
    assert_eq!(walked_past["denials"], 1, "{walked_past}");
    assert_eq!(walked_past["handed_on"], 0, "{walked_past}");
    row("computed.region-body");

    let (ok, md, err) = status(&root, &["--section", "permits"]);
    assert!(ok, "{err}");
    assert!(
        md.contains("| denials | handed on |"),
        "the two are two columns:\n{md}"
    );
    let line = md
        .lines()
        .find(|l| l.contains("permit.stale-build"))
        .unwrap_or_else(|| panic!("no row for the refusal:\n{md}"));
    assert!(line.contains("next candidate"), "and says so: {line}");
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
    assert!(
        row.contains("| 3 | 0 | 1 |"),
        "total, the calls handed on, then this session: {row}"
    );
    let row = md.lines().find(|l| l.contains("ext.no-locks")).unwrap();
    assert!(row.contains("| 0 | 0 | 0 |"), "{row}");
    let (_, md, _) = status(&root, &["--section", "permits"]);
    let row = md.lines().find(|l| l.contains("harness.self")).unwrap();
    assert!(
        row.contains("| 3 | 0 |  |"),
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
    assert!(row.contains("| 3 | 0 | 1 |"), "{row}");
    assert!(
        !text.lines().any(|l| l.contains("ext.no-locks")),
        "only the rules this session fired: {text}"
    );
    let (ok, json, _) = status_with_stdin(&root, &["--session", "--format", "json"], &head);
    assert!(ok);
    let blocks: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(
        blocks["facts"]
            .as_str()
            .unwrap()
            .contains("## permits this session"),
        "this session's denials are the facts block's: {blocks}"
    );
    assert!(
        !blocks["map"].as_str().unwrap().contains("permits"),
        "{blocks}"
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
