//! The git rules: `commit`'s own argv, staging where another hand's
//! merge stands, and a merge that commits itself.

use super::*;

/// `git.no-verify` reads git commit's own argv (#1001): every `deny*.json`
/// under its fixtures is denied and every `allow*.json` let through, under
/// the row as tracked and under the row as proposed. A grep for the word
/// with line numbers, a `sed -n` over it, a `head -n` over a `commit.txt`,
/// a heredoc body naming the bypass and `git log -n 3` are allowed (#517);
/// `-n`, `--no-verify`, `-anm` and a commit after a `cd` are denied. `deny-hooks-path.json` is the proposed row's
/// alone until a person lands it, the tracked row naming no hooks path.
#[test]
fn git_no_verify_reads_git_commit_s_own_argv() {
    let tracked = Manifest::load(workspace()).unwrap();
    let mut proposed = tracked.clone();
    let row = proposed
        .rules
        .iter_mut()
        .find(|r| r.id == "git.no-verify")
        .unwrap();
    row.predicate.with = OneOrMany::Many(vec!["git commit".to_owned(), "git merge".to_owned()]);
    row.predicate.deny_argv = Some(PROPOSED_NO_VERIFY.map(str::to_owned).to_vec());
    let tracked_reads_hooks_path = tracked
        .rule("git.no-verify")
        .and_then(|r| r.predicate.deny_argv.as_ref())
        .is_some_and(|d| d.iter().any(|e| e == "-c core.hooksPath"));
    let tracked_reads_merge = tracked
        .rule("git.no-verify")
        .is_some_and(|r| r.predicate.with.iter().any(|w| w == "git merge"));
    let dir = fixtures().join("rules/git.no-verify");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert!(names.len() >= 10, "{names:?}");
    let denied = |manifest: &Manifest, name: &str| {
        judge(manifest, workspace(), &call(&dir.join(name)))
            .unwrap()
            .findings
            .iter()
            .any(|f| f.rule == "git.no-verify")
    };
    for name in &names {
        let expect = name.starts_with("deny");
        assert_eq!(denied(&proposed, name), expect, "proposed row: {name}");
        let tracked_expect = expect
            && (name != "deny-hooks-path.json" || tracked_reads_hooks_path)
            && (name != "deny-merge.json" || tracked_reads_merge);
        assert_eq!(
            denied(&tracked, name),
            tracked_expect,
            "tracked row: {name}"
        );
    }
}

/// `git.operation` (#2476, as #2530 ruled it): `git add -- <path>`, a
/// commit by pathspec, another hand's `git merge --continue` and a `git
/// restore --staged` are refused in a checkout with `MERGE_HEAD` present,
/// the denial naming the merge and the checkout, and each is let through
/// in one without it; a leading `cd` into the merging checkout is refused
/// from anywhere, every subcommand the widened argv names is refused
/// there, and what reads the index is let through. The walks of
/// `rule_fixtures.rs` hold the fixtures where a merge stands; this adds the
/// checkout without one.
#[test]
fn the_git_operation_rule_refuses_staging_where_another_hand_s_merge_stands() {
    let manifest = Manifest::load(workspace()).unwrap();
    let merging = operating("operation-merging");
    let quiet = checkout("operation-quiet", None);
    let rule = |root: &Path, line: &str| {
        let judged = judge(&manifest, root, &bash(line)).unwrap();
        judged.denial().map(Finding::reason)
    };
    let dir = fixtures().join("rules/git.operation");
    for name in [
        "deny.json",
        "deny-commit.json",
        "deny-continue.json",
        "deny-restore.json",
    ] {
        let judged = judge(&manifest, merging.path(), &call(&dir.join(name))).unwrap();
        let denial = judged
            .denial()
            .unwrap_or_else(|| panic!("{name}: {judged:?}"));
        assert_eq!(denial.rule, "git.operation", "{name}");
        let reason = denial.reason();
        assert!(
            reason.contains(&format!(
                "a merge is in progress in {}",
                merging.path().display()
            )),
            "{reason}"
        );
        let judged = judge(&manifest, quiet.path(), &call(&dir.join(name))).unwrap();
        assert!(
            judged.denial().is_none(),
            "{name} without a merge: {judged:?}"
        );
    }
    let allowed = judge(&manifest, merging.path(), &call(&dir.join("allow.json"))).unwrap();
    assert!(allowed.denial().is_none(), "{allowed:?}");
    let add = "git add -- AGENTS.md";
    assert!(rule(quiet.path(), add).is_none());
    let moved = format!("cd {} && {add}", merging.path().display());
    assert!(rule(quiet.path(), &moved).is_some_and(|r| r.contains("git.operation")));
    for line in [
        "git reset -- AGENTS.md",
        "git apply --cached x.patch",
        "git rm -q --cached -- a",
        "git mv a b",
        "git stage -- a",
        "git cherry-pick --skip",
        "git revert --abort",
        "git rebase --continue",
        "git am --skip",
        "git merge --abort",
    ] {
        let reason = rule(merging.path(), line);
        assert!(
            reason.is_some_and(|r| r.contains("git.operation")),
            "{line}"
        );
    }
    for line in [
        "git status --short",
        "git diff --cached -- AGENTS.md",
        "git log --grep add",
    ] {
        assert!(rule(merging.path(), line).is_none(), "{line}");
    }
}

/// `git.merge-commits-itself` as #2751 ruled it: a merge that commits
/// itself is refused in the main checkout and let through in a linked
/// worktree, where a builder merges `main` into its branch; a neighbour
/// in main does not place a merge `-C` runs in the worktree there.
#[test]
fn a_merge_that_commits_itself_is_refused_in_main_alone() {
    let manifest = Manifest::load(workspace()).unwrap();
    let main = checkout("merge-commits-itself", None);
    Repo::at(&main).git(&["worktree", "add", "-q", "-b", "own", "own"]);
    let linked = main.path().join("own");
    let refused = |root: &Path, line: &str| {
        judge(&manifest, root, &bash(line))
            .unwrap()
            .findings
            .iter()
            .any(|f| f.rule == "git.merge-commits-itself" && f.kind == Kind::Deny)
    };
    assert!(refused(main.path(), "git merge --no-ff t1234"));
    assert!(!refused(main.path(), "git merge --no-ff --no-commit t1234"));
    assert!(!refused(&linked, "git merge main"));
    let apart = format!(
        "git -C {} merge --abort; git -C {} merge main",
        main.path().display(),
        linked.display()
    );
    assert!(!refused(main.path(), &apart));
}
