//! Which checkout a call is judged in: an agent worktree, a branch with
//! a slash, a fixture's own manifest, the main checkout, and the
//! `where` scopes.

use super::*;

#[test]
fn a_file_in_an_agent_worktree_is_judged_against_that_checkout() {
    let manifest = Manifest::load(workspace()).unwrap();
    let worktree = workspace().join(format!(
        ".claude/worktrees/agent-permit-{}",
        std::process::id()
    ));
    lay_worktree(&worktree);
    let text = serde_json::json!({
        "tool_name": "Write",
        "cwd": worktree.to_str().unwrap(),
        "tool_input": {
            "file_path": worktree.join("crates/php_ext/src/lib.rs").to_str().unwrap(),
            "content": "pub struct Facade;\n"
        }
    })
    .to_string();
    let call = Call::parse(&text).unwrap();
    let root = call.workspace(workspace());
    assert_eq!(root, worktree.canonicalize().unwrap_or(worktree.clone()));
    assert_eq!(
        call.path(&root).unwrap(),
        Path::new("crates/php_ext/src/lib.rs")
    );
    let judgement = judge(&manifest, &root, &call).unwrap();
    assert!(
        judgement.denial().is_none(),
        "a worktree is a checkout, not .claude/** of the main one: {judgement:#?}"
    );

    // A Bash call carries no file; its cwd places it.
    let shell = Call::parse(
        &serde_json::json!({"tool_name": "Bash", "cwd": worktree.to_str().unwrap(),
            "tool_input": {"command": "cargo test -p xtask"}})
        .to_string(),
    )
    .unwrap();
    assert_eq!(shell.workspace(workspace()), root);
    // Outside any worktree, the root stands.
    let plain = Call::parse(
        &serde_json::json!({"tool_name": "Bash", "cwd": workspace().to_str().unwrap(),
            "tool_input": {"command": "ls"}})
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        plain.workspace(workspace()),
        workspace().canonicalize().unwrap()
    );
    // A directory under .claude/worktrees without a manifest is not a
    // workspace; its files are the main one's `.claude/**`.
    let stray = workspace().join(".claude/worktrees/notes/todo.md");
    let dir = Call::parse(
        &serde_json::json!({"tool_name": "Write",
            "tool_input": {"file_path": stray.to_str().unwrap(), "content": "x"}})
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        dir.workspace(workspace()),
        workspace().canonicalize().unwrap()
    );
    std::fs::remove_dir_all(&worktree).unwrap();
}

/// A track's worktree is named by its branch, and a branch's name is a
/// path (`t-x/witness`), so the checkout is the first directory under
/// `.claude/worktrees` on the way down that has a `.git`, not the first
/// component alone (harness-design.md §5, #41).
#[test]
fn a_worktree_named_by_a_branch_with_a_slash_is_a_checkout_of_its_own() {
    let track = workspace().join(format!(".claude/worktrees/t-permit-{}", std::process::id()));
    let worktree = track.join("witness");
    lay_worktree(&worktree);
    let call = Call::parse(
        &serde_json::json!({"tool_name": "Edit", "cwd": worktree.to_str().unwrap(),
            "tool_input": {"file_path": worktree.join("seams.toml").to_str().unwrap(),
                "old_string": "a", "new_string": "b"}})
        .to_string(),
    )
    .unwrap();
    let root = call.workspace(workspace());
    assert_eq!(root, worktree.canonicalize().unwrap());
    assert_eq!(call.path(&root).unwrap(), Path::new("seams.toml"));
    std::fs::remove_dir_all(&track).unwrap();
}

/// A fixture that carries a `seams.toml` is a workspace of its own, and a
/// call into it is judged by that manifest and not the workspace's
/// (#57): under the `unsafe.homes` fixture's rule, `ext/pcre/src/ffi/` is
/// a home and `php_unit` is not, which is the fixture's own text and the
/// workspace's rule says neither. The main checkout's own files resolve
/// to the main checkout.
#[test]
fn a_fixture_with_a_manifest_of_its_own_is_judged_by_that_manifest() {
    let dir = fixtures().join("rules/unsafe.homes");
    let inside = edit_of(&dir.join("ext/pcre/src/ffi/mod.rs"));
    let root = inside.workspace(workspace());
    assert_eq!(
        root,
        dir.canonicalize().unwrap(),
        "the fixture is the workspace"
    );
    assert_eq!(
        inside.path(&root).unwrap(),
        Path::new("ext/pcre/src/ffi/mod.rs"),
        "and the path is relative to it"
    );
    let manifest = Manifest::load(&root).unwrap();
    let judged = judge(&manifest, &root, &inside).unwrap();
    assert!(
        judged.denial().is_none(),
        "a home of the fixture's manifest: {judged:#?}"
    );
    let outside = edit_of(&dir.join("crates/php_unit/src/lib.rs"));
    assert_eq!(outside.workspace(workspace()), root);
    let denial = judge(&manifest, &root, &outside).unwrap();
    let denial = denial.denial().expect("outside the fixture's homes");
    assert_eq!(denial.rule, "unsafe.homes");
    assert!(
        denial.text.contains("(fixture)"),
        "the fixture's text, not the workspace's: {}",
        denial.text
    );
    // Under the workspace's manifest no fixture source is in a home, which
    // is the freeze #57 lifts.
    let workspaces = Manifest::load(workspace()).unwrap();
    assert!(
        judge(&workspaces, workspace(), &inside)
            .unwrap()
            .denial()
            .is_some(),
        "the workspace's rule would deny the home"
    );

    // A call without a file is placed by its cwd in a checkout alone: a
    // shell standing in the fixture is judged by the checkout's manifest,
    // which carries the shell rules the fixture's does not.
    let shell = Call::parse(
        &serde_json::json!({"tool_name": "Bash", "cwd": dir.to_str().unwrap(),
            "tool_input": {"command": "git commit --no-verify -m x"}})
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        shell.workspace(workspace()),
        workspace().canonicalize().unwrap()
    );
    assert_eq!(
        judge(&workspaces, workspace(), &shell)
            .unwrap()
            .denial()
            .map(|f| f.rule.clone())
            .as_deref(),
        Some("git.no-verify")
    );
}

/// The main checkout: a file of the workspace itself resolves to the
/// workspace, and the path is relative to it.
#[test]
fn a_file_of_the_main_checkout_is_judged_by_its_manifest() {
    let own = edit_of(&workspace().join("crates/php_heap/src/lib.rs"));
    assert_eq!(
        own.workspace(workspace()),
        workspace().canonicalize().unwrap()
    );
    assert_eq!(
        own.path(workspace()).unwrap(),
        Path::new("crates/php_heap/src/lib.rs")
    );
    let manifest = Manifest::load(workspace()).unwrap();
    assert!(
        judge(&manifest, workspace(), &own)
            .unwrap()
            .denial()
            .is_none(),
        "php_heap is a home of the workspace's manifest"
    );
}

/// `where = "ticketed"` (#1149): the rule judges a call in a checkout
/// whose branch records a ticket, a builder's worktree, and not one in a
/// checkout that records none, main's, where the merge check runs; a
/// leading `cd` or `pushd` moves the call to the checkout it names.
#[test]
fn a_rule_where_ticketed_judges_only_a_checkout_whose_branch_records_a_ticket() {
    let manifest = probe(|rule| rule.within = Where::Ticketed);
    let ticketed = checkout("where-ticketed", Some(1149));
    let main = checkout("where-main", None);
    let line = "cargo xtask phpt --census";
    assert!(denied(&manifest, ticketed.path(), line));
    assert!(!denied(&manifest, main.path(), line));
    let moved = format!("cd {} && {line}", ticketed.path().display());
    assert!(denied(&manifest, main.path(), &moved));
    let moved = format!("cd {} && {line}", main.path().display());
    assert!(!denied(&manifest, ticketed.path(), &moved));
    // #2607: a `cd` to a variable the line assigned, and a `pushd`, move
    // it as a `cd` does, and a `popd` moves it back.
    let assigned = format!("W={} && cd \"$W\" && {line}", ticketed.path().display());
    assert!(denied(&manifest, main.path(), &assigned));
    let pushed = format!("pushd {} && {line}", ticketed.path().display());
    assert!(denied(&manifest, main.path(), &pushed));
    let popped = format!("pushd {} && popd && {line}", ticketed.path().display());
    assert!(!denied(&manifest, main.path(), &popped));
    // Without `where`, both.
    let manifest = probe(|_| {});
    assert!(denied(&manifest, main.path(), line));
}

/// #2995: a `cd` joined to what follows by anything but `&&` may fail,
/// or runs in a shell of its own, so `where = "ticketed"` judges what
/// follows where the line stood as well, at the root; a program moved by
/// its own words is judged where they move it.
#[test]
fn a_rule_where_ticketed_judges_past_a_change_that_may_not_move_the_line() {
    let manifest = probe(|rule| {
        rule.within = Where::Ticketed;
        rule.predicate.deny_pattern = Some(r"\bgit\s+add\b".to_owned());
    });
    let ticketed = checkout("where-ticketed-joints", Some(2995));
    let main = checkout("where-main-joints", None);
    let (wt, root) = (ticketed.path(), main.path());
    let denied = |root: &Path, line: String| denied(&manifest, root, &line);
    // Judged at the root, the ticketed checkout, though the `cd` names
    // main's; joined by `&&`, the `cd` moves it.
    let other = root.display();
    assert!(denied(wt, format!("cd {other}; git add x")));
    assert!(denied(wt, format!("cd {other} | true; git add x")));
    assert!(!denied(wt, format!("cd {other} && git add x")));
    // From main: piped, the `cd` moves nothing; sequenced, it may.
    let into = wt.display();
    assert!(!denied(root, format!("cd {into} | true; git add x")));
    assert!(denied(root, format!("cd {into}; git add x")));
    // A program's own words move it.
    assert!(denied(root, format!("env -C {into} git add x")));
    assert!(denied(root, format!("env --chdir {into} git add x")));
    assert!(!denied(wt, format!("env -C {other} git add x")));
    assert!(denied(root, format!("GIT_WORK_TREE={into} git add x")));
}

/// `where = "main"` (#2751): the rule judges a call in the repository's
/// main checkout and not one in a linked worktree, wherever the line
/// stands when a leading `cd` moves it; of a line's commands, the one the
/// pattern hits is the one asked where it runs.
#[test]
fn a_rule_where_main_judges_only_the_main_checkout() {
    let manifest = probe(|rule| rule.within = Where::Main);
    let main = checkout("where-main-only", None);
    Repo::at(&main).git(&["worktree", "add", "-q", "-b", "own", "own"]);
    let linked = main.path().join("own");
    let line = "cargo xtask phpt --census";
    assert!(denied(&manifest, main.path(), line));
    assert!(!denied(&manifest, &linked, line));
    let moved = format!("cd {} && {line}", linked.display());
    assert!(!denied(&manifest, main.path(), &moved));
    let moved = format!("cd {} && {line}", main.path().display());
    assert!(denied(&manifest, &linked, &moved));
}
