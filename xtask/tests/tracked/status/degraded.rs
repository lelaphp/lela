//! A torn log line and an unreadable reading file, each named and
//! degrading only its own row.

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
