//! The permits section: denials per session and in total, this session
//! marked, and a sessions cell that does not grow.

use super::*;

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
        row.contains("| 3 | 0 | 0 | 0 | 1 |"),
        "total, the calls judged, at pre-commit, handed on, then this session: {row}"
    );
    let row = md.lines().find(|l| l.contains("ext.no-locks")).unwrap();
    assert!(row.contains("| 0 | 0 | 0 | 0 | 0 |"), "{row}");
    let md = status(&root, &["--section", "permits"]).stdout;
    let row = md.lines().find(|l| l.contains("harness.self")).unwrap();
    assert!(
        row.contains("| 3 | 0 | 0 | 0 |  |"),
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

/// A rule's `sessions` cell names its newest three sessions and counts
/// the rest: the append-only log holds every session a rule ever
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
        row.split(" | ").nth(9).unwrap().to_owned()
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
