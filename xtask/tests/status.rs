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

/// The rules section in Markdown is each rule's id, ADR, predicate and
/// reading; the text its failure prints is under `--uncut` and in JSON, so
/// the map carries the rules without it (#1772).
#[test]
fn rules_prints_each_rule_with_its_adr_and_the_failure_text_under_uncut() {
    let root = scratch("rules");
    let text = "Extension state is request-scoped, immutable engine-scoped or process-scoped";
    let (ok, md, err) = status(&root, &["--section", "rules"]);
    assert!(ok, "{err}");
    assert!(
        md.starts_with("| rule | adr | predicate | reading |\n"),
        "{md}"
    );
    assert!(
        md.contains("| ext.no-locks | ADR-0005 | deny_types | no reading |\n"),
        "{md}"
    );
    assert!(md.contains("harness.self"), "{md}");
    assert!(
        !md.contains(text),
        "the failure's text is not the map's: {md}"
    );

    let (ok, uncut, err) = status(&root, &["--section", "rules", "--uncut"]);
    assert!(ok, "{err}");
    assert!(
        uncut.starts_with("| rule | adr | predicate | reading | text |\n"),
        "{uncut}"
    );
    assert!(uncut.contains(text), "{uncut}");

    let (ok, json, _) = status(&root, &["--section", "rules", "--format", "json"]);
    assert!(ok);
    assert!(json.contains(text), "{json}");
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

/// `--tracked` renders a section against the tree alone, as a tracked
/// region carries it (harness-design.md §5, revision 6; #630): a reading
/// the store holds is still the plain section's, the tracked table has no
/// `reading` column, which could only say `no reading` there (#2377), and
/// a section whose rows are this checkout's alone is refused.
#[test]
fn tracked_renders_no_reading_whatever_the_store_holds() {
    let root = scratch("tracked");
    let mut reading = Reading::new("permit", "harness.self");
    reading.status = Status::Fail;
    reading.rule = "harness.self".into();
    Store::of_workspace(&root)
        .record(std::slice::from_ref(&reading))
        .unwrap();
    let row = "| harness.self |  | deny |";

    let (ok, live, err) = status(&root, &["--section", "rules"]);
    assert!(ok, "{err}");
    assert!(live.contains(&format!("{row} fail |")), "{live}");
    assert!(
        live.starts_with("| rule | adr | predicate | reading |\n"),
        "{live}"
    );
    let (ok, tracked, err) = status(&root, &["--section", "rules", "--tracked"]);
    assert!(ok, "{err}");
    assert!(
        tracked.starts_with("| rule | adr | predicate |\n|---|---|---|\n"),
        "{tracked}"
    );
    assert!(tracked.contains(&format!("{row}\n")), "{tracked}");
    assert!(!tracked.contains("reading"), "{tracked}");
    for section in ["locks", "bundle"] {
        let (ok, out, err) = status(&root, &["--section", section, "--tracked"]);
        assert!(ok, "{section}: {err}");
        assert!(!out.contains("reading"), "{section}: {out}");
    }

    for section in ["git", "permits", "phpt"] {
        let (ok, out, err) = status(&root, &["--section", section, "--tracked"]);
        assert!(!ok, "{section}: {out}");
        assert!(
            err.contains(&format!("the {section} section is this checkout's")),
            "{section}: {err}"
        );
    }
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

/// The modules section is each item lock's lines by the module that
/// defines them, from the groups of the seam's api-check reading, the
/// widest first with its share of the lock and the reading's commit
/// (#1795); a seam whose reading
/// keeps no groups has no row, and with none at all the section has no
/// reading.
#[test]
fn modules_renders_each_lock_by_its_defining_modules_the_widest_first() {
    let root = scratch("modules");
    let store = Store::of_workspace(&root);
    let (ok, md, err) = status(&root, &["--section", "modules"]);
    assert!(ok, "{err}");
    assert_eq!(md, "no reading\n");

    let mut api = Reading::new("api-check", "api");
    api.status = Status::Pass;
    api.numbers.insert("items".into(), 4.into());
    api.commit = "21ad0fb8c0ffee".into();
    api.group("php_engine::api::values", 1).unwrap();
    api.group("php_engine::api::request", 3).unwrap();
    let mut heap = Reading::new("api-check", "heap");
    heap.status = Status::Pass;
    heap.numbers.insert("items".into(), 9.into());
    store.record(&[api, heap]).unwrap();

    let (ok, md, err) = status(&root, &["--section", "modules"]);
    assert!(ok, "{err}");
    assert_eq!(
        md,
        "| seam | module | items | share | commit |\n|---|---|---|---|---|\n\
         | api | php_engine::api::request | 3 | 75.0% | 21ad0fb8 |\n\
         | api | php_engine::api::values | 1 | 25.0% | 21ad0fb8 |\n"
    );

    let (ok, json, err) = status(&root, &["--section", "modules", "--format", "json"]);
    assert!(ok, "{err}");
    let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(rows[0]["lock"], "api.lock");
    assert_eq!(rows[0]["module"], "php_engine::api::request");
    assert_eq!(rows[0]["items"], 3);
    assert_eq!(rows.as_array().unwrap().len(), 2);
}

/// The crates section counts the manifest's `[[crate]]` rows by the
/// directory each is laid down in, from the manifest alone, so a region
/// that renders it is held by `inputs=seams.toml` (#2250); every row, with
/// whether its directory holds a `Cargo.toml`, is under `--uncut` and in
/// JSON.
#[test]
fn crates_counts_the_manifest_rows_by_directory_and_lists_each_under_uncut() {
    let root = scratch("crates");
    std::fs::create_dir_all(root.join("crates/php_heap")).unwrap();
    std::fs::write(root.join("crates/php_heap/Cargo.toml"), "").unwrap();
    let manifest = Manifest::load(&root).unwrap();
    let count = |dir: &str, feature: bool| {
        manifest
            .crates
            .iter()
            .filter(|c| c.path().starts_with(dir) && (!feature || c.feature.is_some()))
            .count()
    };

    let (ok, md, err) = status(&root, &["--section", "crates"]);
    assert!(ok, "{err}");
    assert!(
        md.starts_with("| directory | crates | behind a feature |\n|---|---|---|\n"),
        "{md}"
    );
    for dir in ["crates", "ext", "sapi"] {
        let line = format!(
            "| {dir}/ | {} | {} |\n",
            count(dir, false),
            count(dir, true)
        );
        assert!(md.contains(&line), "{line}in {md}");
    }
    assert!(md.ends_with("| xtask | 1 | 0 |\n"), "{md}");

    let (ok, uncut, err) = status(&root, &["--section", "crates", "--uncut"]);
    assert!(ok, "{err}");
    assert!(
        uncut.starts_with("| crate | directory | feature | created |\n"),
        "{uncut}"
    );
    assert!(
        uncut.contains("| php_heap | crates/php_heap |  | yes |\n"),
        "{uncut}"
    );
    assert!(
        uncut.contains("| php_jit | crates/php_jit | jit | no |\n"),
        "{uncut}"
    );
    assert!(
        uncut.contains("| ext/pcre | ext/pcre |  | no |\n"),
        "{uncut}"
    );
    assert_eq!(uncut.lines().count(), manifest.crates.len() + 2, "{uncut}");

    let (ok, json, err) = status(&root, &["--section", "crates", "--format", "json"]);
    assert!(ok, "{err}");
    let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), manifest.crates.len());
    let heap = rows.iter().find(|r| r["crate"] == "php_heap").unwrap();
    assert_eq!(heap["directory"], "crates/php_heap");
    assert_eq!(heap["created"], true);
    assert_eq!(heap["feature"], serde_json::Value::Null);
    let jit = rows.iter().find(|r| r["crate"] == "php_jit").unwrap();
    assert_eq!(jit["feature"], "jit");
    assert_eq!(jit["created"], false);
}

/// The edges section is the manifest's `[[edge]]` rows in its order, each
/// with where it reaches, how the row qualifies it and its ADRs, from the
/// manifest alone (#2250); in JSON the rows are the manifest's as it
/// parses them.
#[test]
fn edges_renders_every_edge_row_with_its_qualifiers_and_adrs() {
    let root = scratch("edges");
    let manifest = Manifest::load(&root).unwrap();

    let (ok, md, err) = status(&root, &["--section", "edges"]);
    assert!(ok, "{err}");
    assert!(
        md.starts_with("| from | to | as | adr |\n|---|---|---|---|\n"),
        "{md}"
    );
    assert_eq!(md.lines().count(), manifest.edges.len() + 2, "{md}");
    for edge in &manifest.edges {
        let to: Vec<&str> = edge.to.iter().collect();
        let head = format!("| {} | {} | ", edge.from, to.join(", "));
        assert!(md.contains(&head), "{head}in {md}");
    }
    let row = |from: &str| {
        md.lines()
            .find(|l| l.starts_with(&format!("| {from} |")))
            .unwrap_or_else(|| panic!("no edge from {from}: {md}"))
            .to_owned()
    };
    assert!(
        row("php_engine::{executor,kernels}").contains("| denied |"),
        "{md}"
    );
    assert!(row("php_ext").contains("| re-exports only |"), "{md}");
    assert!(
        row("php_bundle").ends_with("| registration order | ADR-0004, ADR-0006 |"),
        "{md}"
    );
    assert!(
        row("php_jit").contains("| imports only what tier.lock lists | ADR-0006 |"),
        "{md}"
    );
    assert!(row("php_sapi").contains("| through sapi.lock |"), "{md}");
    assert!(row("ext/*").contains("| these only | ADR-0004 |"), "{md}");

    let (ok, json, err) = status(&root, &["--section", "edges", "--format", "json"]);
    assert!(ok, "{err}");
    let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(rows, serde_json::to_value(&manifest.edges).unwrap());
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

    // The items column is what the tree's lock lists, `__private`'s lines
    // apart as `api_items` holds them, and not the reading's count; the
    // ceiling column is the tracked manifest's `[budget] api_items`,
    // whatever a person has it at (#338, #631).
    std::fs::create_dir_all(root.join("crates/php_ext")).unwrap();
    std::fs::write(
        root.join("crates/php_ext/api.lock"),
        "# a header\nvalues::Zval\nvalues::Array\n\n__private::Entry\nrequest::Request\n",
    )
    .unwrap();
    let ceiling = Manifest::load(&root).unwrap().budget.api_items.to_string();
    let (ok, md, _) = status(&root, &["--section", "locks"]);
    assert!(ok);
    assert!(
        md.contains(&format!("| api | api.lock | 3 | {ceiling} | pass |")),
        "{md}"
    );
    assert!(!md.contains("301"), "{md}");
    assert!(
        md.contains("| heap | heap.lock |  |  | no reading |"),
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

    let (ok, md, err) = status(&root, &["--section", "budget", "--uncut"]);
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
    assert!(
        md.contains(
            "| ext/pcre | skip |  |  | skip | not yet created; the subset holds 165 tests |"
        ),
        "the control folds into its subset's row: {md}"
    );
    assert!(!md.contains("| ext/pcre@control |"), "{md}");
    assert!(!md.contains("crates not yet created"), "{md}");
}

/// The sections that grow by a row per crate render the rows a session
/// acts on and count the settled rest in one line (#1608): a failure, a
/// skip, a row with no reading, a budget within a tenth of its ceiling and
/// a phpt subset whose comparison fails are rows; a pass is a count. Fifty
/// more passing crates are the same section, `--uncut` is every row, and
/// the JSON is every reading whatever the flag.
#[test]
fn the_settled_rows_of_a_section_fold_into_one_line_that_does_not_grow() {
    let root = scratch("settled");
    let store = Store::of_workspace(&root);
    let record = |instrument: &str, scope: &str, status: Status, numbers: &[(&str, i64)]| {
        let mut r = Reading::new(instrument, scope);
        r.status = status;
        for (k, v) in numbers {
            r.numbers.insert((*k).into(), (*v).into());
        }
        r.message = format!("{scope}: {}", status.as_str());
        store.record(std::slice::from_ref(&r)).unwrap();
    };
    let ceiling = |name: &str| {
        let manifest = Manifest::load(&root).unwrap();
        i64::try_from(manifest.crate_named(name).unwrap().budget).unwrap()
    };
    let spare = ceiling("php_syntax") / 2;
    let near = ceiling("php_heap") - 1;
    record("budget", "php_syntax", Status::Pass, &[("lines", spare)]);
    record("budget", "php_heap", Status::Pass, &[("lines", near)]);
    record("budget", "php_unit", Status::Fail, &[("lines", 1)]);
    let (ok, md, err) = status(&root, &["--section", "budget"]);
    assert!(ok, "{err}");
    assert!(
        !md.contains("| php_syntax |"),
        "a pass with room is counted: {md}"
    );
    assert!(
        md.contains("| php_heap |"),
        "within a tenth of its ceiling: {md}"
    );
    assert!(
        md.contains("| php_unit |") && md.contains("| php_compile |"),
        "{md}"
    );
    let line = md.lines().last().unwrap();
    assert!(
        line.starts_with("1 more crates pass")
            && line.ends_with("`cargo xtask status --section budget --uncut`."),
        "{line}"
    );
    let (ok, whole, _) = status(&root, &["--section", "budget", "--uncut"]);
    assert!(
        ok && whole.contains("| php_syntax |") && !whole.contains("--uncut"),
        "{whole}"
    );
    let (ok, json, _) = status(&root, &["--section", "budget", "--format", "json"]);
    assert!(ok && json.contains("\"php_syntax\""), "{json}");

    record("witness", "ext/broken", Status::Fail, &[("failed", 1)]);
    record("witness", "ext/missing", Status::Skip, &[]);
    record("witness", "ext/a0", Status::Pass, &[("passed", 3)]);
    let (ok, before, _) = status(&root, &["--section", "witness"]);
    assert!(ok, "{before}");
    for n in 1..=50 {
        record(
            "witness",
            &format!("ext/a{n}"),
            Status::Pass,
            &[("passed", 3)],
        );
    }
    let (ok, after, _) = status(&root, &["--section", "witness"]);
    assert!(ok);
    assert_eq!(
        before.replace("1 more crates", "51 more crates"),
        after,
        "fifty passing crates are one number of the line"
    );
    assert!(
        after.contains("| ext/broken |") && after.contains("| ext/missing |"),
        "{after}"
    );
    assert!(!after.contains("| ext/a7 |"), "{after}");
    let (_, uncut, _) = status(&root, &["--section", "witness", "--uncut"]);
    assert_eq!(
        uncut.lines().filter(|l| l.starts_with("| ext/a")).count(),
        51
    );

    record("witness", "ext/broken", Status::Pass, &[("passed", 1)]);
    record("witness", "ext/missing", Status::Pass, &[("passed", 1)]);
    let (ok, settled, _) = status(&root, &["--section", "witness"]);
    assert!(ok);
    assert_eq!(
        settled, "53 crates pass; every row: `cargo xtask status --section witness --uncut`.\n",
        "a section with every row settled is the line alone"
    );

    record("phpt", "ext/date", Status::Pass, &[("passed", 9)]);
    record("phpt", "ext/date@against", Status::Fail, &[("lost", 1)]);
    record("phpt", "ext/spl", Status::Pass, &[("passed", 9)]);
    record("phpt", "ext/spl@against", Status::Pass, &[("held", 9)]);
    let (ok, md, err) = status(&root, &["--section", "phpt"]);
    assert!(ok, "{err}");
    assert!(
        md.contains("| ext/date | pass |") && md.contains("| fail lost=1 |"),
        "a pass below its target is a row: {md}"
    );
    assert!(!md.contains("| ext/spl |"), "{md}");
    assert!(md.ends_with("1 more subsets pass, none below the target it is held to; every row: `cargo xtask status --section phpt --uncut`.\n"), "{md}");
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

/// The pinned php a runner pours (#2253): its version, keg path and md5
/// as the pin states them, so the oracle job reads the pin off this
/// section and never parses `oracle.toml` a second time.
#[test]
fn oracle_states_the_pinned_php_a_runner_pours() {
    let root = scratch("oracle-binary");
    let oracle = Manifest::load(workspace()).unwrap().pin.oracle;
    let path = oracle.path.display().to_string();
    let json = oracle_json(&root, None, None);
    assert_eq!(json["binary"]["version"], oracle.version.as_str());
    assert_eq!(json["binary"]["path"], path.as_str());
    assert_eq!(json["binary"]["md5"], oracle.md5.as_str());
    let (ok, md, err) = status_with_pin(&root, &["--section", "oracle"], None, None);
    assert!(ok, "{err}");
    assert!(
        md.contains(&format!(
            "| binary |  | {path} | php {}, md5 {} |",
            oracle.version, oracle.md5
        )),
        "{md}"
    );
}

/// The pin's tag and commit as `[source]` states them (#66), beside its
/// path: the section names the pin whole, which the corpus section clones
/// at, so no reader parses `oracle.toml` for either.
#[test]
fn oracle_states_the_pins_tag_and_commit() {
    let root = scratch("oracle-source");
    let source = Manifest::load(workspace()).unwrap().pin.source;
    let json = oracle_json(&root, None, None);
    assert_eq!(json["pin"]["tag"], source.tag.as_str());
    assert_eq!(json["pin"]["commit"], source.commit.as_str());
    let (ok, md, err) = status_with_pin(&root, &["--section", "oracle"], None, None);
    assert!(ok, "{err}");
    assert!(
        md.contains(&format!("| tag |  | {} | the pin's |", source.tag)),
        "{md}"
    );
    assert!(
        md.contains(&format!("| commit |  | {} | the pin's |", source.commit)),
        "{md}"
    );
}

/// The corpus CI checks out (#1713): the pin's tag, commit and count of
/// `.phpt` files, the sparse patterns of the corpus's roots, and a cache
/// key of the commit and a digest of the patterns, the same on every call,
/// where the targets under the pin are kept (#1733), and the hosts the lane
/// runs a test through, which CI builds (#1908); the Markdown carries the
/// same facts, and nothing is written.
#[test]
fn corpus_states_the_pin_the_patterns_and_the_key_they_make() {
    let root = scratch("corpus");
    let source = Manifest::load(workspace()).unwrap().pin.source;
    let (ok, json, err) = status(&root, &["--section", "corpus", "--format", "json"]);
    assert!(ok, "{err}");
    let corpus: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(corpus["tag"], source.tag.as_str());
    assert_eq!(corpus["commit"], source.commit.as_str());
    assert_eq!(corpus["phpt_files"], source.phpt_files);
    let patterns: Vec<&str> = corpus["patterns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    for pattern in ["/Zend/tests/", "/tests/", "/ext/*/", "/sapi/*/tests/"] {
        assert!(patterns.contains(&pattern), "{pattern}: {patterns:?}");
    }
    let key = corpus["key"].as_str().unwrap();
    let digest = key
        .strip_prefix(&format!("php-src-{}-", source.commit))
        .unwrap_or_else(|| panic!("{key} names the commit"));
    assert!(
        digest.len() == 12 && digest.chars().all(|c| c.is_ascii_hexdigit()),
        "{key}"
    );
    let pin = Manifest::load(workspace()).unwrap().pin;
    assert_eq!(
        corpus["targets"],
        format!(
            "target/harness/phpt-runs/pin-{}-{}",
            pin.source.tag, pin.oracle.md5
        ),
        "the targets are kept under the pin's tag and the oracle's digest"
    );
    // The hosts the lane hands a test to (#1908), the binary under test
    // first: each `sapi/*` crate of the workspace, by the package and the
    // binary its own Cargo.toml names, which CI builds and spells nowhere.
    let hosts: Vec<(String, String, String)> = corpus["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            let field = |k: &str| h[k].as_str().unwrap().to_owned();
            (field("crate"), field("package"), field("binary"))
        })
        .collect();
    assert_eq!(
        hosts.first().map(|h| h.0.as_str()),
        Some("sapi/cli"),
        "the binary under test first: {hosts:?}"
    );
    for (krate, package, binary) in &hosts {
        let cargo: toml::Table = toml::from_str(
            &std::fs::read_to_string(workspace().join(krate).join("Cargo.toml")).unwrap(),
        )
        .unwrap();
        assert_eq!(cargo["package"]["name"].as_str(), Some(package.as_str()));
        let bins = cargo["bin"].as_array().unwrap();
        assert!(
            bins.iter()
                .any(|b| b["name"].as_str() == Some(binary.as_str())),
            "{krate} builds {binary}"
        );
    }
    let (_, again, _) = status(&root, &["--section", "corpus", "--format", "json"]);
    assert_eq!(again, json, "the key is the patterns', not the call's");

    let (ok, md, err) = status(&root, &["--section", "corpus"]);
    assert!(ok, "{err}");
    assert!(md.contains(&format!("| key | {key} |")), "{md}");
    assert!(md.contains("/sapi/*/tests/"), "{md}");
    assert!(
        md.contains(&format!(
            "| hosts | {} |",
            hosts
                .iter()
                .map(|h| h.1.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        )),
        "{md}"
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
    let message = cells[5].trim_end();
    assert_eq!(message.chars().count(), 120, "{message}");
    assert!(
        message.starts_with("691 of 871 missed: t000.phpt"),
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

/// The week's ticket closes (#1157), from the stores alone: a close is
/// written into the main worktree's store by the merge that lands it, and
/// a withdrawal into the checkout's where its session ran, so a worktree
/// counts both. A ready record, a read, a refused close and a close older
/// than the week count nothing.
#[test]
fn tickets_counts_the_weeks_closes_in_the_main_worktrees_store_and_the_checkouts() {
    let main = scratch("tickets");
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&main)
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
        "first",
    ]);
    run(&["worktree", "add", "-q", "-b", "t1", "wt"]);
    let checkout = main.join("wt");
    for file in ["seams.toml", "oracle.toml", "docs/divergence.toml"] {
        std::fs::create_dir_all(checkout.join(file).parent().unwrap()).unwrap();
        std::fs::copy(main.join(file), checkout.join(file)).unwrap();
    }
    let now = xtask::reading::now().unwrap();
    let ticket = |scope: &str, status: Status, numbers: &[(&str, i64)], at: &str| {
        let mut r = Reading::new("ticket", scope);
        r.status = status;
        r.at = if at.is_empty() {
            now
        } else {
            at.parse().unwrap()
        };
        for (key, n) in numbers {
            r.numbers.insert((*key).to_owned(), (*n).into());
        }
        r
    };
    Store::of_workspace(&main)
        .record(&[
            ticket("1", Status::Pass, &[("opens", 2), ("withdrawn", 0)], ""),
            ticket("2", Status::Pass, &[("opens", 1), ("withdrawn", 0)], ""),
            ticket("3", Status::Pass, &[("opens", 4)], ""),
            ticket("4", Status::Fail, &[("opens", 3), ("withdrawn", 0)], ""),
            ticket(
                "5",
                Status::Pass,
                &[("opens", 9), ("withdrawn", 0)],
                "2020-01-01T00:00:00Z",
            ),
        ])
        .unwrap();
    Store::of_workspace(&checkout)
        .record(&[
            ticket("6", Status::Pass, &[("opens", 0), ("withdrawn", 1)], ""),
            ticket("7", Status::Pass, &[("criteria", 2)], ""),
        ])
        .unwrap();

    let (ok, md, err) = status(&checkout, &["--section", "tickets"]);
    assert!(ok, "{err}");
    assert_eq!(
        md,
        "the last 7 days: 3 closes, 1 of them withdrawals, opened 3 tickets, 1.00 per close\n"
    );
    let (ok, json, err) = status(&checkout, &["--section", "tickets", "--format", "json"]);
    assert!(ok, "{err}");
    let json: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(json["closes"], 3);
    assert_eq!(json["withdrawn"], 1);
    assert_eq!(json["opens"], 3);
    assert_eq!(json["opens_per_close"], 1.0);
    // The main worktree reads its own store once, and has no withdrawal.
    let (ok, md, err) = status(&main, &["--section", "tickets"]);
    assert!(ok, "{err}");
    assert!(
        md.contains(": 2 closes, 0 of them withdrawals, opened 3 tickets"),
        "{md}"
    );
    let (ok, _, err) = status(&main, &["--section", "tickets", "--tracked"]);
    assert!(
        !ok && err.contains("--tracked renders the tree alone"),
        "{err}"
    );
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

/// #822: a working tree with more changed paths than the section prints
/// keeps twenty of them, `STATUS_LINES` in `status.rs`, below the
/// branch's line and names how many it left out; the JSON keeps the same
/// lines with the number beside them.
#[test]
fn git_cuts_the_status_at_twenty_paths_and_names_the_rest() {
    let root = scratch("git-cut");
    for args in [
        &["init", "-q", "-b", "main"][..],
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "cut",
        ],
    ] {
        let out = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success());
    }
    for i in 0..40 {
        std::fs::write(root.join(format!("untracked-{i:02}.txt")), "x").unwrap();
    }
    // The scratch's own seams.toml, oracle.toml and docs/ are untracked too.
    let (ok, md, err) = status(&root, &["--section", "git"]);
    assert!(ok, "{err}");
    let paths = md.lines().filter(|l| l.starts_with("?? ")).count();
    assert_eq!(paths, 20, "{md}");
    assert!(
        md.contains("\n… and 23 more; git status --short\n```"),
        "{md}"
    );
    let (ok, json, err) = status(&root, &["--section", "git", "--format", "json"]);
    assert!(ok, "{err}");
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["status"].as_array().map(Vec::len), Some(21), "{json}");
    assert_eq!(value["status_omitted"], 23, "{json}");
}

/// #787: a track's reading of a subset is the one at its tip or its
/// nearest ancestor on the track. A branch cut behind the tip, whose
/// worktree's log `worktree retire` appends into the track's store after
/// the track's own readings, stands for nothing, and neither does a
/// reading of an older commit written last; the JSON the dashboard's data
/// step reads says the same, with the commit each reading was taken at.
#[test]
fn phpt_takes_the_reading_at_the_track_tip_and_never_a_retired_branchs() {
    let root = scratch("chain");
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "-q", "-b", "t-x"]);
    git(&["commit", "-q", "--allow-empty", "-m", "cut"]);
    let cut = git(&["rev-parse", "--short", "HEAD"]);
    git(&["switch", "-q", "-c", "behind"]);
    git(&["commit", "-q", "--allow-empty", "-m", "the branch"]);
    let branch = git(&["rev-parse", "--short", "HEAD"]);
    git(&["switch", "-q", "t-x"]);
    git(&["commit", "-q", "--allow-empty", "-m", "the tip"]);
    let tip = git(&["rev-parse", "--short", "HEAD"]);

    let phpt = |scope: &str, commit: &str, at: &str, passed: i64| {
        let mut r = Reading::new("phpt", scope);
        r.status = Status::Fail;
        r.commit = commit.into();
        r.at = at.parse().unwrap();
        r.numbers.insert("passed".into(), passed.into());
        r
    };
    let track = Store::of_workspace(&root);
    let worktree = Store::at(root.join("worktree-store"));
    track
        .record(&[phpt("ext/date", &tip, "2026-09-24T16:44:00Z", 613)])
        .unwrap();
    track
        .record(&[phpt("ext/date", &cut, "2026-09-24T16:50:00Z", 400)])
        .unwrap();
    worktree
        .record(&[
            phpt("ext/date", &branch, "2026-09-24T17:35:00Z", 0),
            phpt("ext/spl", &branch, "2026-09-24T17:36:00Z", 1),
        ])
        .unwrap();
    assert_eq!(track.land(&worktree).unwrap(), 2);

    let (ok, json, err) = status(&root, &["--section", "phpt", "--format", "json"]);
    assert!(ok, "{err}");
    let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "the branch's ext/spl stands for nothing: {json}"
    );
    assert_eq!(rows[0]["scope"], "ext/date");
    assert_eq!(rows[0]["reading"]["commit"], tip.as_str(), "{json}");
    assert_eq!(rows[0]["reading"]["numbers"]["passed"], 613, "{json}");
    let (ok, md, _) = status(&root, &["--section", "phpt"]);
    assert!(
        ok && md.contains("passed=613") && !md.contains("ext/spl"),
        "{md}"
    );
}

/// `--every --log` renders every phpt reading of a log it is handed, one
/// versioned line each, in the log's order (harness-design.md §4.6,
/// #1745): a whole line with a field this build does not know, a lean row
/// of the published history, a torn line counted on stderr and skipped, a
/// scope outside the grammar, and the other instruments' lines left out.
#[test]
fn every_phpt_reading_of_a_log_handed_in_is_one_versioned_line() {
    let root = scratch("every-log");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/status-phpt");
    let log = fixture.join("log.jsonl");
    let expected = std::fs::read_to_string(fixture.join("expected.jsonl")).unwrap();
    let every = ["--section", "phpt", "--format", "jsonl", "--every", "--log"];

    let (ok, lines, err) = status(&root, &[&every[..], &[log.to_str().unwrap()]].concat());
    assert!(ok, "{err}");
    assert_eq!(lines, expected);
    assert!(err.contains("1 line unreadable at 4"), "{err}");
    for line in lines.lines() {
        let row: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(row["v"], 1, "{line}");
    }

    let text = std::fs::read_to_string(&log).unwrap();
    let (ok, piped, err) = status_with_stdin(&root, &[&every[..], &["-"]].concat(), &text);
    assert!(ok, "{err}");
    assert_eq!(piped, expected, "the standard input reads as the file does");

    let (ok, _, err) = status(&root, &[&every[..], &["no-such-log.jsonl"]].concat());
    assert!(!ok && err.contains("no-such-log.jsonl"), "{err}");
}

/// `--every --log` renders every budget, api-check (`locks` by its other
/// name) and witness reading of
/// a log it is handed as one versioned line each, in the log's order
/// (#1763), as it does a phpt reading: a whole line, a lean row of the
/// published history, a torn line counted on stderr and skipped, and the
/// other instruments' lines left out. Each fixture's `expected.jsonl` is
/// what the dashboard's tests read as the rows xtask renders.
#[test]
fn every_budget_api_check_and_witness_reading_of_a_log_handed_in_is_one_versioned_line() {
    let root = scratch("every-sections");
    for (section, instrument, torn) in [
        ("budget", "budget", 8),
        ("api-check", "api-check", 6),
        ("witness", "witness", 5),
    ] {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("fixtures/status-{section}"));
        let log = fixture.join("log.jsonl");
        let expected = std::fs::read_to_string(fixture.join("expected.jsonl")).unwrap();
        let every = [
            "--section",
            section,
            "--format",
            "jsonl",
            "--every",
            "--log",
        ];

        let (ok, lines, err) = status(&root, &[&every[..], &[log.to_str().unwrap()]].concat());
        assert!(ok, "{section}: {err}");
        assert_eq!(lines, expected, "{section}");
        assert!(
            err.contains(&format!("1 line unreadable at {torn}")),
            "{section}: {err}"
        );
        for line in lines.lines() {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(
                (&row["v"], &row["instrument"]),
                (&serde_json::json!(1), &serde_json::json!(instrument)),
                "{line}"
            );
        }

        let text = std::fs::read_to_string(&log).unwrap();
        let (ok, piped, err) = status_with_stdin(&root, &[&every[..], &["-"]].concat(), &text);
        assert!(ok, "{section}: {err}");
        assert_eq!(
            piped, expected,
            "{section}: the standard input reads as the file does"
        );
    }
}

/// Without `--log`, `--every` renders the store's own log, every reading
/// and none picked, where the section without it renders the one that
/// stands, as it did; and each flag is refused where it means nothing.
#[test]
fn every_renders_the_stores_log_and_the_section_without_it_is_unchanged() {
    let root = scratch("every-store");
    let store = Store::of_workspace(&root);
    let phpt = |commit: &str, at: &str, passed: i64| {
        let mut r = Reading::new("phpt", "ext/date");
        r.status = Status::Pass;
        r.commit = commit.into();
        r.at = at.parse().unwrap();
        r.numbers.insert("passed".into(), passed.into());
        r.numbers.insert("tests".into(), passed.into());
        r.numbers.insert("run".into(), passed.into());
        r
    };
    store
        .record(&[
            phpt("", "2026-09-24T16:44:00Z", 400),
            phpt("", "2026-09-24T16:50:00Z", 613),
        ])
        .unwrap();

    let (ok, lines, err) = status(
        &root,
        &["--section", "phpt", "--format", "jsonl", "--every"],
    );
    assert!(ok, "{err}");
    let passed: Vec<i64> = lines
        .lines()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["p"]
                .as_i64()
                .unwrap()
        })
        .collect();
    assert_eq!(passed, [400, 613], "{lines}");

    let (ok, json, err) = status(&root, &["--section", "phpt", "--format", "json"]);
    assert!(ok, "{err}");
    let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 1, "{json}");
    assert_eq!(rows[0]["reading"]["numbers"]["passed"], 613, "{json}");

    for (args, says) in [
        (&["--section", "phpt", "--format", "jsonl"][..], "--every"),
        (&["--section", "phpt", "--every"][..], "--format jsonl"),
        (
            &["--section", "test", "--format", "jsonl", "--every"][..],
            "test",
        ),
        (&["--section", "phpt", "--log", "-"][..], "--every"),
    ] {
        let (ok, _, err) = status(&root, args);
        assert!(!ok && err.contains(says), "{args:?}: {err}");
    }
}

/// A scratch workspace whose store holds the phpt readings the parity
/// tests read, and whose pin's path is `pin`.
fn parity_store(name: &str, pin: &Path) -> Tree {
    let root = scratch_with_pin_path(name, pin);
    let phpt = |scope: &str, counts: &[(&str, i64)]| {
        let mut r = Reading::new("phpt", scope);
        r.status = Status::Pass;
        r.at = "2026-10-03T12:00:00Z".parse().unwrap();
        for (key, n) in counts {
            r.numbers.insert((*key).into(), (*n).into());
        }
        r
    };
    let held = |n: i64, met: i64| {
        [
            ("tests", n + 10),
            ("run", n + 10),
            ("target", n),
            ("met", met),
            ("outside", 10),
        ]
    };
    Store::of_workspace(&root)
        .record(&[
            phpt("Zend/tests", &held(100, 90)),
            phpt(
                "ext/ctype",
                &[&held(48, 47)[..], &[("predated", 2)]].concat(),
            ),
            phpt("ext/json", &[("tests", 88), ("run", 88), ("passed", 88)]),
            phpt("ext/ctype@control", &[("tests", 50), ("run", 50)]),
            phpt("ext/ctype@against", &[("held", 47)]),
            phpt("ext/ctype@force", &held(1000, 1000)),
            phpt("ext/standard/tests/serialize", &held(500, 500)),
            phpt("ext/pgsql", &held(3, 3)),
            phpt("ext/enchant", &held(20, 20)),
            phpt("sapi/cli", &held(99, 99)),
            // A second host's run of tests other suites count: the HTTP-served
            // lane under its host's name, whose row owns no subset (#2334).
            phpt("sapi/makala", &held(267, 260)),
            phpt("ext/lexbor", &[("tests", 0), ("run", 0)]),
            phpt(
                "ext/pcre",
                &[("tests", 165), ("run", 0), ("engine_absent", 1)],
            ),
        ])
        .unwrap();
    root
}

/// The `parity` section (design §1, #1761): each suite parity counts with
/// met-of-target off the phpt reading that stands for it, the total over
/// the suites held to a target, and the suites outside parity with the
/// ruling that put them there or as not built; a control, a lane, a
/// comparison and a narrower run are no suite's own, and a reading held to
/// no target is in no total. A host's reading whose row owns no subset,
/// `sapi/makala`'s run of the CGI-shaped tests over HTTP, is no suite and
/// none outside, so a test met through two hosts counts once (#2334).
#[test]
fn parity_states_met_of_target_per_suite_and_in_total_and_names_the_suites_outside() {
    let root = parity_store("parity", Path::new(NO_TREE));
    let (ok, json, err) = status_with_pin(
        &root,
        &["--section", "parity", "--format", "json"],
        None,
        None,
    );
    assert!(ok, "{err}");
    let parity: serde_json::Value = serde_json::from_str(&json).unwrap();
    let suite = |name: &str| {
        parity["suites"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["suite"] == name)
            .unwrap_or_else(|| panic!("no suite {name}: {json}"))
            .clone()
    };
    assert_eq!(
        suite("Zend/tests")["target"],
        serde_json::json!({ "n": 100, "met": 90, "outside": 10, "predated": 0 })
    );
    assert_eq!(
        suite("ext/ctype")["target"],
        serde_json::json!({ "n": 48, "met": 47, "outside": 10, "predated": 2 })
    );
    assert_eq!(suite("ext/ctype")["state"], "held");
    assert_eq!(
        suite("sapi/cli")["target"],
        serde_json::json!({ "n": 99, "met": 99, "outside": 10, "predated": 0 }),
        "a host's suite is its row's name, the scope its reading is taken under"
    );
    for (name, state) in [
        ("ext/json", "untargeted"),
        ("ext/pcre", "unmeasured"),
        ("ext/lexbor", "empty"),
        ("sapi/cgi", "missing"),
    ] {
        assert_eq!(suite(name)["state"], state, "{name}: {json}");
        assert_eq!(suite(name)["target"], serde_json::Value::Null, "{name}");
    }
    assert_eq!(suite("ext/json")["reading"]["status"], "pass");
    assert_eq!(suite("sapi/cgi")["reading"], serde_json::Value::Null);
    let suites = parity["suites"].as_array().unwrap();
    assert!(
        suites
            .iter()
            .all(|s| !s["suite"].as_str().unwrap().contains('@')
                && s["suite"] != "ext/standard/tests/serialize"
                && s["suite"] != "ext/pgsql"
                && s["suite"] != "sapi/makala"),
        "{json}"
    );
    assert_eq!(
        parity["total"],
        serde_json::json!({ "suites": suites.len(), "held": 3, "met": 236, "target": 247 })
    );
    let outside = parity["outside"].as_array().unwrap();
    assert!(
        outside.contains(
            &serde_json::json!({ "suite": "ext/pgsql", "why": "ruling", "ruling": 1167 })
        ),
        "{json}"
    );
    assert!(
        outside
            .contains(&serde_json::json!({ "suite": "ext/ldap", "why": "ruling", "ruling": 1167 })),
        "a suite ruled out is named whether or not a reading measured it: {json}"
    );
    assert!(
        outside.contains(&serde_json::json!({ "suite": "ext/enchant", "why": "unbuilt" })),
        "{json}"
    );
    assert_eq!(
        outside.len(),
        8,
        "the seven under #1167 and ext/enchant: {json}"
    );
    assert_eq!(
        parity["unbuilt_from"], "readings",
        "without the pinned tree, the unbuilt are the measured ones alone: {json}"
    );
}

/// A pin's path where no tree stands, so a parity test reads as it would
/// on a machine without the pinned php-src.
const NO_TREE: &str = "/nowhere/pinned/php-src";

/// With the pinned tree present (#1847), the section names as not built
/// every `ext/*` and `sapi/*` suite of the tree that holds a `.phpt` and
/// that no row owns or `[parity] out` rules out, whether or not a reading
/// measured it; a suite a row owns, `sapi/embed`'s among them though its
/// row owns no subset yet, one ruled out, a directory holding no test, and
/// the core's suites are not.
#[test]
fn parity_names_every_unbuilt_suite_of_the_pinned_tree() {
    let tree = fixture_tree("parity-tree");
    for (under, file) in [
        ("Zend/tests", "a.phpt"),
        ("tests/lang", "a.phpt"),
        ("ext/ctype/tests", "a.phpt"),
        ("ext/pgsql/tests", "a.phpt"),
        ("ext/enchant/tests", "a.phpt"),
        ("ext/oci8/tests/nested", "a.phpt"),
        ("ext/hollow/src", "hollow.c"),
        ("sapi/cli/tests", "a.phpt"),
        ("sapi/pi3web/tests", "a.phpt"),
        ("sapi/embed/tests", "a.phpt"),
        ("sapi/bare", "README"),
    ] {
        std::fs::create_dir_all(tree.join(under)).unwrap();
        std::fs::write(tree.join(under).join(file), "").unwrap();
    }
    let root = parity_store("parity-tree", &tree);
    let (ok, json, err) = status_with_pin(
        &root,
        &["--section", "parity", "--format", "json"],
        None,
        None,
    );
    assert!(ok, "{err}");
    let parity: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parity["unbuilt_from"], "tree", "{json}");
    let unbuilt: Vec<&str> = parity["outside"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|o| o["why"] == "unbuilt")
        .map(|o| o["suite"].as_str().unwrap())
        .collect();
    assert_eq!(
        unbuilt,
        ["ext/enchant", "ext/oci8", "sapi/pi3web"],
        "ext/oci8 and sapi/pi3web no reading measured: {json}"
    );
    assert!(
        parity["outside"].as_array().unwrap().contains(
            &serde_json::json!({ "suite": "ext/pgsql", "why": "ruling", "ruling": 1167 })
        ),
        "a suite ruled out stays under its ruling: {json}"
    );

    let (ok, md, err) = status_with_pin(&root, &["--section", "parity"], None, None);
    assert!(ok, "{err}");
    assert!(
        md.contains(
            "Outside parity, not built by the pinned php: ext/enchant, ext/oci8, sapi/pi3web."
        ),
        "{md}"
    );
}

/// The same section as Markdown: a row per suite, the total, and the
/// suites outside parity by ruling and as not built.
#[test]
fn parity_renders_as_markdown_for_a_session() {
    let root = parity_store("parity-md", Path::new(NO_TREE));
    let (ok, md, err) = status_with_pin(&root, &["--section", "parity"], None, None);
    assert!(ok, "{err}");
    assert!(
        md.contains("| ext/ctype | held | 47 | 48 | 10 | 2 | pass |"),
        "{md}"
    );
    assert!(
        md.contains("| ext/json | untargeted |  |  |  |  | pass |"),
        "{md}"
    );
    assert!(
        md.contains("| sapi/cgi | missing |  |  |  |  | no reading |"),
        "{md}"
    );
    assert!(md.contains("236 of 247 target tests met"), "{md}");
    assert!(
        md.contains("#1167") && md.contains("not built by the pinned php: ext/enchant"),
        "{md}"
    );
    assert!(
        md.contains("named only where a reading measured them"),
        "without the pinned tree the section says its unbuilt list is the measured ones: {md}"
    );
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

/// A rule's `sessions` cell names its newest three sessions and counts
/// the rest (#1618): the append-only log holds every session a rule ever
/// denied, so a cell that listed them all grew with each one for good.
/// Fifty more sessions move one number of the cell; `--uncut` and the
/// JSON carry every session.
#[test]
fn the_sessions_cell_of_permits_does_not_grow_as_sessions_are_added() {
    let root = scratch("sessions-cut");
    let store = Store::of_workspace(&root);
    let deny = |n: usize| {
        let mut denial = Reading::new("permit", "harness.self");
        denial.rule = "harness.self".into();
        denial.status = Status::Fail;
        denial.message = "Edit seams.toml: edited by a person.".into();
        denial.session = format!("{n:08x}-0000-4000-8000-000000000000");
        store.record(std::slice::from_ref(&denial)).unwrap();
    };
    let cell = |md: &str| {
        let row = md.lines().find(|l| l.contains("| harness.self |")).unwrap();
        row.split(" | ").nth(7).unwrap().to_owned()
    };
    for n in 1..=5 {
        deny(n);
    }
    let (ok, md, err) = status(&root, &["--section", "permits"]);
    assert!(ok, "{err}");
    let before = cell(&md);
    assert_eq!(
        before, "00000005=1 00000004=1 00000003=1 +2 more",
        "the newest three, the rest counted"
    );
    for n in 6..=55 {
        deny(n);
    }
    let (ok, md, _) = status(&root, &["--section", "permits"]);
    assert!(ok);
    let after = cell(&md);
    assert_eq!(after, "00000037=1 00000036=1 00000035=1 +52 more");
    assert_eq!(before.len() + 1, after.len(), "one digit more: {after}");

    let (ok, whole, _) = status(&root, &["--section", "permits", "--uncut"]);
    assert!(ok);
    let whole = cell(&whole);
    assert_eq!(whole.split(' ').count(), 55, "{whole}");
    assert!(!whole.contains("more"), "{whole}");

    let (ok, json, _) = status(&root, &["--section", "permits", "--format", "json"]);
    assert!(ok);
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let row = value["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["rule"] == "harness.self")
        .unwrap();
    assert_eq!(row["sessions"].as_array().unwrap().len(), 55);
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
    assert_eq!(
        row, "| harness.self | 1 |",
        "the rule and this session's count"
    );
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
    std::fs::create_dir_all(store.root().join("log")).unwrap();
    std::fs::copy(&fixture, store.root().join("log/permit.jsonl")).unwrap();

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
        md.contains("log: 2 lines unreadable at 2, 3."),
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
    assert!(text.contains("log: 2 lines unreadable at 2, 3."), "{text}");
}

/// A reading file a crash left empty, or cut short, costs its own row and
/// no more (#1235): the section renders every other reading, the row of
/// the file that is no reading says it has none, the file is named by its
/// path on stderr, and a section that reads no reading file renders as
/// it did. Before, the one file failed every section, `divergence`
/// included.
#[test]
fn an_unreadable_reading_file_degrades_only_its_own_row_and_is_named() {
    let root = scratch("unreadable-file");
    let store = Store::of_workspace(&root);
    let mut heap = Reading::new("budget", "php_heap");
    heap.status = Status::Pass;
    heap.numbers.insert("lines".into(), 7941.into());
    store.record(std::slice::from_ref(&heap)).unwrap();
    std::fs::write(store.root().join("budget/php_unit.toml"), "").unwrap();
    let cut = store.root().join("unsafe-audit/ext/calendar.toml");
    std::fs::create_dir_all(cut.parent().unwrap()).unwrap();
    std::fs::write(&cut, "instrument = \"unsafe-au").unwrap();

    let (ok, md, err) = status(&root, &["--section", "budget", "--uncut"]);
    assert!(ok, "{err}");
    let row = |name: &str| {
        md.lines()
            .find(|l| l.starts_with(&format!("| {name} |")))
            .unwrap_or_else(|| panic!("no row for {name}: {md}"))
            .to_owned()
    };
    assert!(row("php_heap").contains("7941"), "{md}");
    assert!(row("php_unit").contains("no reading"), "{md}");
    assert!(
        err.contains(
            "target/harness: 2 readings unreadable at budget/php_unit.toml, unsafe-audit/ext/calendar.toml"
        ),
        "{err}"
    );

    let (ok, json, err) = status(&root, &["--section", "budget", "--format", "json"]);
    assert!(ok, "{err}");
    let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(
        rows.as_array()
            .unwrap()
            .iter()
            .any(|r| r["crate"] == "php_heap")
    );

    let (ok, md, err) = status(&root, &["--section", "divergence"]);
    assert!(ok, "{err}");
    assert!(md.contains("total"), "{md}");
}
