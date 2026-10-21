use super::*;

/// A torn line in the log (#56) degrades the `permits` section rather than
/// the session: the section renders the readings around it and names the
/// line, and `--session` prints the map and the local facts with the note
/// under this session's block, where before both failed on the line.
#[test]
fn a_torn_log_line_is_named_and_the_rest_of_the_section_and_the_session_render() {
    let root = tracked_scratch("torn");
    std::fs::write(root.join("AGENTS.md"), "# The map\n").unwrap();
    let fixture = xtask::package_dir().join("fixtures/store/torn.jsonl");
    let store = Store::of_workspace(&root);
    std::fs::create_dir_all(store.root().join("log")).unwrap();
    std::fs::copy(&fixture, store.root().join("log/permit.jsonl")).unwrap();

    let md = status(&root, &["--section", "permits"]).ok();
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

    let json = status(&root, &["--section", "permits", "--format", "json"]).ok();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["unreadable"], serde_json::json!([2, 3]));

    let this = "5e55100c-0000-4000-8000-000000000000";
    let head = format!(r#"{{"session_id":"{this}","hook_event_name":"SessionStart"}}"#);
    let text = status_with_stdin(&root, &["--session"], &head).ok();
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
    let root = tracked_scratch("unreadable-file");
    let store = Store::of_workspace(&root);
    let mut heap = Reading::new("budget", "php_heap");
    heap.status = Status::Pass;
    heap.numbers.insert("lines".into(), 7941.into());
    store.record(std::slice::from_ref(&heap)).unwrap();
    std::fs::write(store.root().join("budget/php_unit.toml"), "").unwrap();
    let cut = store.root().join("unsafe-audit/ext/calendar.toml");
    std::fs::create_dir_all(cut.parent().unwrap()).unwrap();
    std::fs::write(&cut, "instrument = \"unsafe-au").unwrap();

    let ran = status(&root, &["--section", "budget", "--uncut"]);
    assert_eq!(ran.code(), 0, "{ran}");
    let md = ran.stdout;
    let err = ran.stderr;
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

    let json = status(&root, &["--section", "budget", "--format", "json"]).ok();
    let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(
        rows.as_array()
            .unwrap()
            .iter()
            .any(|r| r["crate"] == "php_heap")
    );

    let md = status(&root, &["--section", "divergence"]).ok();
    assert!(md.contains("total"), "{md}");
}

/// The edges section is the manifest's `[[edge]]` rows in its order, each
/// with where it reaches, how the row qualifies it and its ADRs, from the
/// manifest alone (#2250); in JSON the rows are the manifest's as it
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
/// that renders it is held by `inputs=seams.toml` (#2250); every row, with
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

#[test]
fn locks_and_budget_render_the_readings_the_instruments_wrote() {
    let root = tracked_scratch("readings");
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
    let md = status(&root, &["--section", "locks"]).ok();
    assert!(
        md.contains(&format!("| api | api.lock | 3 | {ceiling} | pass |")),
        "{md}"
    );
    assert!(!md.contains("301"), "{md}");
    assert!(
        md.contains("| heap | heap.lock |  |  | no reading |"),
        "{md}"
    );

    let md = status(&root, &["--section", "budget"]).ok();
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

    let json = status(&root, &["--section", "budget", "--format", "json"]).ok();
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

/// The modules section is each item lock's lines by the module that
/// defines them, from the groups of the seam's api-check reading, the
/// widest first with its share of the lock and the reading's commit
/// (#1795); a seam whose reading
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

/// Three denials of one rule across two sessions: the rule's row counts
/// them per session, newest session first, and in total (§10 rule 6).
#[test]
fn permits_counts_denials_per_session_and_in_total() {
    let root = tracked_scratch("sessions");
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

    let md = status(&root, &["--section", "permits"]).ok();
    let row = md.lines().find(|l| l.contains("harness.self")).unwrap();
    assert!(row.contains("| 3 |"), "three denials in total: {row}");
    assert!(
        row.contains("9a8b7c6d=1 3f1c0d2a=2"),
        "per session, the newest first: {row}"
    );

    let json = status(&root, &["--section", "permits", "--format", "json"]).ok();
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
    let json = status_with_pin(
        &root,
        &["--section", "parity", "--format", "json"],
        None,
        None,
    )
    .ok();
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

    let md = status_with_pin(&root, &["--section", "parity"], None, None).ok();
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
    let md = status_with_pin(&root, &["--section", "parity"], None, None).ok();
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

/// "This session" (harness-design.md §4.6): `--session-id` names it, and
/// the rule's row shows its count apart; `--session` takes the id from
/// the hook's stdin head and appends this session's denials after the
/// local facts, nothing when the session has none.
#[test]
fn permits_marks_this_session_and_the_session_block_carries_it() {
    let root = tracked_scratch("this-session");
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
    let md = status(&root, &["--section", "permits", "--session-id", this]).ok();
    let row = md.lines().find(|l| l.contains("harness.self")).unwrap();
    assert!(
        row.contains("| 3 | 0 | 1 |"),
        "total, the calls handed on, then this session: {row}"
    );
    let row = md.lines().find(|l| l.contains("ext.no-locks")).unwrap();
    assert!(row.contains("| 0 | 0 | 0 |"), "{row}");
    let md = status(&root, &["--section", "permits"]).stdout;
    let row = md.lines().find(|l| l.contains("harness.self")).unwrap();
    assert!(
        row.contains("| 3 | 0 |  |"),
        "no session named, no count: {row}"
    );

    let json = status(
        &root,
        &[
            "--section",
            "permits",
            "--session-id",
            this,
            "--format",
            "json",
        ],
    )
    .ok();
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
    let text = status_with_stdin(&root, &["--session"], &head).ok();
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
    let json = status_with_stdin(&root, &["--session", "--format", "json"], &head).ok();
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
    let text = status_with_stdin(&root, &["--session"], quiet).ok();
    assert!(
        !text.contains("this session"),
        "a session with no denial adds nothing: {text}"
    );
    let ran = status_with_stdin(&root, &["--session"], "not json");
    assert_eq!(
        ran.code(),
        0,
        "a head that is not JSON is no session, not an error: {ran}"
    );
    let text = ran.stdout;
    assert!(!text.contains("this session"), "{text}");
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
    let json = status_with_pin(
        &root,
        &["--section", "parity", "--format", "json"],
        None,
        None,
    )
    .ok();
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

/// The rules section in Markdown is each rule's id, ADR, predicate and
/// reading; the text its failure prints is under `--uncut` and in JSON, so
/// the map carries the rules without it (#1772).
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
/// at the end of the section, "N crates not yet created", naming them
/// (#64): twenty such rows carry one fact, and the session block has a
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

/// The sections that grow by a row per crate render the rows a session
/// acts on and count the settled rest in one line (#1608): a failure, a
/// skip, a row with no reading, a budget within a tenth of its ceiling and
/// a phpt subset whose comparison fails are rows; a pass is a count. Fifty
/// more passing crates are the same section, `--uncut` is every row, and
/// the JSON is every reading whatever the flag.
#[test]
fn the_settled_rows_of_a_section_fold_into_one_line_that_does_not_grow() {
    let root = tracked_scratch("settled");
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
    let md = status(&root, &["--section", "budget"]).ok();
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
    let whole = status(&root, &["--section", "budget", "--uncut"]).ok();
    assert!(
        whole.contains("| php_syntax |") && !whole.contains("--uncut"),
        "{whole}"
    );
    let json = status(&root, &["--section", "budget", "--format", "json"]).ok();
    assert!(json.contains("\"php_syntax\""), "{json}");

    record("witness", "ext/broken", Status::Fail, &[("failed", 1)]);
    record("witness", "ext/missing", Status::Skip, &[]);
    record("witness", "ext/a0", Status::Pass, &[("passed", 3)]);
    let before = status(&root, &["--section", "witness"]).ok();
    for n in 1..=50 {
        record(
            "witness",
            &format!("ext/a{n}"),
            Status::Pass,
            &[("passed", 3)],
        );
    }
    let after = status(&root, &["--section", "witness"]).ok();
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
    let uncut = status(&root, &["--section", "witness", "--uncut"]).stdout;
    assert_eq!(
        uncut.lines().filter(|l| l.starts_with("| ext/a")).count(),
        51
    );

    record("witness", "ext/broken", Status::Pass, &[("passed", 1)]);
    record("witness", "ext/missing", Status::Pass, &[("passed", 1)]);
    let settled = status(&root, &["--section", "witness"]).ok();
    assert_eq!(
        settled, "53 crates pass; every row: `cargo xtask status --section witness --uncut`.\n",
        "a section with every row settled is the line alone"
    );

    record("phpt", "ext/date", Status::Pass, &[("passed", 9)]);
    record("phpt", "ext/date@against", Status::Fail, &[("lost", 1)]);
    record("phpt", "ext/spl", Status::Pass, &[("passed", 9)]);
    record("phpt", "ext/spl@against", Status::Pass, &[("held", 9)]);
    let md = status(&root, &["--section", "phpt"]).ok();
    assert!(
        md.contains("| ext/date | pass |") && md.contains("| fail lost=1 |"),
        "a pass below its target is a row: {md}"
    );
    assert!(!md.contains("| ext/spl |"), "{md}");
    assert!(md.ends_with("1 more subsets pass, none below the target it is held to; every row: `cargo xtask status --section phpt --uncut`.\n"), "{md}");
}

/// A rule's `sessions` cell names its newest three sessions and counts
/// the rest (#1618): the append-only log holds every session a rule ever
/// denied, so a cell that listed them all grew with each one for good.
/// Fifty more sessions move one number of the cell; `--uncut` and the
/// JSON carry every session.
#[test]
fn the_sessions_cell_of_permits_does_not_grow_as_sessions_are_added() {
    let root = tracked_scratch("sessions-cut");
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
    let md = status(&root, &["--section", "permits"]).ok();
    let before = cell(&md);
    assert_eq!(
        before, "00000005=1 00000004=1 00000003=1 +2 more",
        "the newest three, the rest counted"
    );
    for n in 6..=55 {
        deny(n);
    }
    let md = status(&root, &["--section", "permits"]).ok();
    let after = cell(&md);
    assert_eq!(after, "00000037=1 00000036=1 00000035=1 +52 more");
    assert_eq!(before.len() + 1, after.len(), "one digit more: {after}");

    let whole = status(&root, &["--section", "permits", "--uncut"]).ok();
    let whole = cell(&whole);
    assert_eq!(whole.split(' ').count(), 55, "{whole}");
    assert!(!whole.contains("more"), "{whole}");

    let json = status(&root, &["--section", "permits", "--format", "json"]).ok();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let row = value["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["rule"] == "harness.self")
        .unwrap();
    assert_eq!(row["sessions"].as_array().unwrap().len(), 55);
}

/// `--tracked` renders a section against the tree alone, as a tracked
/// region carries it (harness-design.md §5, revision 6; #630): a reading
/// the store holds is still the plain section's, the tracked table has no
/// `reading` column, which could only say `no reading` there (#2377), and
/// a section whose rows are this checkout's alone is refused.
#[test]
fn tracked_renders_no_reading_whatever_the_store_holds() {
    let root = tracked_scratch("tracked");
    let mut reading = Reading::new("permit", "harness.self");
    reading.status = Status::Fail;
    reading.rule = "harness.self".into();
    Store::of_workspace(&root)
        .record(std::slice::from_ref(&reading))
        .unwrap();
    let row = "| harness.self |  | deny |";

    let live = status(&root, &["--section", "rules"]).ok();
    assert!(live.contains(&format!("{row} fail |")), "{live}");
    assert!(
        live.starts_with("| rule | adr | predicate | reading |\n"),
        "{live}"
    );
    let tracked = status(&root, &["--section", "rules", "--tracked"]).ok();
    assert!(
        tracked.starts_with("| rule | adr | predicate |\n|---|---|---|\n"),
        "{tracked}"
    );
    assert!(tracked.contains(&format!("{row}\n")), "{tracked}");
    assert!(!tracked.contains("reading"), "{tracked}");
    for section in ["locks", "bundle"] {
        let ran = status(&root, &["--section", section, "--tracked"]);
        assert_eq!(ran.code(), 0, "{section}: {ran}");
        let out = ran.stdout;
        assert!(!out.contains("reading"), "{section}: {out}");
    }

    for section in ["git", "permits", "phpt"] {
        let ran = status(&root, &["--section", section, "--tracked"]);
        assert_ne!(ran.code(), 0, "{section}: {ran}");
        let err = ran.stderr;
        assert!(
            err.contains(&format!("the {section} section is this checkout's")),
            "{section}: {err}"
        );
    }
}

/// The volatile sections of the local file (harness-design.md §5) render
/// the readings of an instrument that does not exist yet as "no reading",
/// and one that was written as its row.
#[test]
fn the_local_sections_render_from_the_store_alone() {
    let root = tracked_scratch("local");
    for section in ["phpt", "witness", "mutants", "ticket", "permits", "bundle"] {
        let ran = status(&root, &["--section", section]);
        assert_eq!(ran.code(), 0, "{section}: {ran}");
        let md = ran.stdout;
        assert!(md.contains("no reading"), "{section}: {md}");
    }

    let store = Store::of_workspace(&root);
    let mut phpt = Reading::new("phpt", "Zend/tests");
    phpt.status = Status::Fail;
    phpt.numbers.insert("passed".into(), 9.into());
    phpt.numbers.insert("failed".into(), 1.into());
    phpt.message = "Zend/tests: 1 of 10 failed".into();
    store.record(std::slice::from_ref(&phpt)).unwrap();
    let md = status(&root, &["--section", "phpt"]).ok();
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
    let md = status(&root, &["--section", "ticket"]).ok();
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
    let md = status(&root, &["--section", "permits"]).ok();
    let row = md.lines().find(|l| l.contains("harness.self")).unwrap();
    assert!(row.contains("| 2 |"), "two denials in the log: {row}");
    assert!(
        md.contains("ext.no-locks") && md.contains("| 0 |"),
        "a rule that never fired says so: {md}"
    );

    let json = status(&root, &["--section", "permits", "--format", "json"]).ok();
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
