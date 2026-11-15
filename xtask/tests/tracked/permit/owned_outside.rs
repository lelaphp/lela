//! `checkout.owner` over a file outside the workspace: the row
//! has neither a scope nor an `adding`, so it reaches an `Edit` or a
//! `Write` outside the workspace and places it where the file
//! lands, which a checkout's owner record makes another session's or no
//! session's.

use super::*;

const RULE: &str = "checkout.owner";

/// `tool` on `file`, carrying what either tool reads, from a session no
/// owner record names.
fn write_by_another_session(tool: &str, file: &Path) -> Call {
    let text = serde_json::json!({
        "session_id": "another-session",
        "tool_name": tool,
        "tool_input": {
            "file_path": file.to_str().unwrap(),
            "old_string": "a", "new_string": "b", "content": "b"
        }
    })
    .to_string();
    Call::parse(&text).unwrap()
}

/// Judged against a scratch workspace, an `Edit` and a `Write` of a file
/// in a scratch checkout `the-owner` owns are each refused by the row,
/// whose denial names that checkout and its owner; the same two writes
/// into a scratch checkout no record names are judged by the row and
/// draw no finding of it, so the refusal is the owner record's and not
/// the file's being outside the workspace.
#[test]
fn an_edit_or_write_outside_the_workspace_into_another_session_s_checkout_is_refused() {
    let manifest = Manifest::load(workspace()).unwrap();
    let root = scratch("owned-outside-root");
    let owned_by = owned("owned-outside-owned");
    let unowned = checkout("owned-outside-unowned", None);
    for checkout in [&owned_by, &unowned] {
        write(checkout.path(), "notes.md", "a\n");
    }
    for tool in ["Edit", "Write"] {
        let call = write_by_another_session(tool, &owned_by.path().join("notes.md"));
        let judged = judge(&manifest, root.path(), &call).unwrap();
        let denial = judged
            .denial()
            .unwrap_or_else(|| panic!("{tool} into the owned checkout: {judged:?}"));
        assert_eq!(denial.rule, RULE, "{tool}: {judged:?}");
        let reason = denial.reason();
        assert!(reason.contains("session the-owner's"), "{tool}: {reason}");
        assert!(
            reason.contains(&owned_by.path().display().to_string()),
            "{tool}: {reason}"
        );

        let call = write_by_another_session(tool, &unowned.path().join("notes.md"));
        let judged = judge(&manifest, root.path(), &call).unwrap();
        assert!(
            judged.judged.iter().any(|id| id == RULE),
            "{tool}: the row reaches the outside file: {judged:?}"
        );
        assert!(
            judged.findings.iter().all(|f| f.rule != RULE),
            "{tool} into the unowned checkout: {judged:?}"
        );
    }
}
