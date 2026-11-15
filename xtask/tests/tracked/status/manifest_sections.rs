//! The sections the manifest renders: edges, crates, modules and rules,
//! and the rows of crates not yet created.

use super::*;

/// The edges section is the manifest's `[[edge]]` rows in its order, each
/// with where it reaches, how the row qualifies it and its ADRs, from the
/// manifest alone; in JSON the rows are the manifest's as it
/// parses them.
#[test]
fn edges_renders_every_edge_row_with_its_qualifiers_and_adrs() {
    let root = tracked_scratch("edges");
    let manifest = Manifest::load(&root).unwrap();

    let md = status(&root, &["--section", "edges"]).ok();
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

    let json = status(&root, &["--section", "edges", "--format", "json"]).ok();
    let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(rows, serde_json::to_value(&manifest.edges).unwrap());
}

/// The crates section counts the manifest's `[[crate]]` rows by the
/// directory each is laid down in, from the manifest alone, so a region
/// that renders it is held by `inputs=seams.toml`; every row, with
/// whether its directory holds a `Cargo.toml`, is under `--uncut` and in
/// JSON.
#[test]
fn crates_counts_the_manifest_rows_by_directory_and_lists_each_under_uncut() {
    let root = tracked_scratch("crates");
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

    let md = status(&root, &["--section", "crates"]).ok();
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

    let uncut = status(&root, &["--section", "crates", "--uncut"]).ok();
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

    let json = status(&root, &["--section", "crates", "--format", "json"]).ok();
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

/// The modules section is each item lock's lines by the module that
/// defines them, from the groups of the seam's api-check reading, the
/// widest first with its share of the lock and the reading's commit;
/// a seam whose reading
/// keeps no groups has no row, and with none at all the section has no
/// reading.
#[test]
fn modules_renders_each_lock_by_its_defining_modules_the_widest_first() {
    let root = tracked_scratch("modules");
    let store = Store::of_workspace(&root);
    let md = status(&root, &["--section", "modules"]).ok();
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

    let md = status(&root, &["--section", "modules"]).ok();
    assert_eq!(
        md,
        "| seam | module | items | share | commit |\n|---|---|---|---|---|\n\
         | api | php_engine::api::request | 3 | 75.0% | 21ad0fb8 |\n\
         | api | php_engine::api::values | 1 | 25.0% | 21ad0fb8 |\n"
    );

    let json = status(&root, &["--section", "modules", "--format", "json"]).ok();
    let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(rows[0]["lock"], "api.lock");
    assert_eq!(rows[0]["module"], "php_engine::api::request");
    assert_eq!(rows[0]["items"], 3);
    assert_eq!(rows.as_array().unwrap().len(), 2);
}

/// The rules section in Markdown is each rule's id, ADR, predicate and
/// reading; the text its failure prints is under `--uncut` and in JSON, so
/// the map carries the rules without it.
#[test]
fn rules_prints_each_rule_with_its_adr_and_the_failure_text_under_uncut() {
    let root = tracked_scratch("rules");
    let text = "Extension state is request-scoped, immutable engine-scoped or process-scoped";
    let md = status(&root, &["--section", "rules"]).ok();
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

    let uncut = status(&root, &["--section", "rules", "--uncut"]).ok();
    assert!(
        uncut.starts_with("| rule | adr | predicate | reading | text |\n"),
        "{uncut}"
    );
    assert!(uncut.contains(text), "{uncut}");

    let json = status(&root, &["--section", "rules", "--format", "json"]).ok();
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

/// Rows whose reading says the crate is not yet created fold into one row
/// at the end of the section, "N crates not yet created", naming them:
/// twenty such rows carry one fact, and the session block has a
/// cap. A lone one stays as it is, and a crate that is measured keeps its
/// row; the fold reads the message alone, as budget, witness and phpt
/// write it.
#[test]
fn the_rows_of_crates_not_yet_created_fold_into_one() {
    let root = tracked_scratch("uncreated");
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

    let md = status(&root, &["--section", "budget", "--uncut"]).ok();
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

    let json = status(&root, &["--section", "budget", "--format", "json"]).ok();
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
    let md = status(&root, &["--section", "phpt"]).ok();
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
