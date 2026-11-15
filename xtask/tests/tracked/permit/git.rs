//! The git rules: `commit`'s own argv, staging where another hand's
//! merge stands, and a merge that commits itself.

use super::*;

/// `git.no-verify` reads git commit's own argv: every `deny*.json`
/// under its fixtures is denied and every `allow*.json` let through, under
/// the row as tracked and under the row as proposed. A grep for the word
/// with line numbers, a `sed -n` over it, a `head -n` over a `commit.txt`,
/// a heredoc body naming the bypass and `git log -n 3` are allowed;
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

/// `git.operation` (ruled 2026-10-10): `git add -- <path>`, a commit by
/// pathspec, another hand's `git merge --continue` and a `git restore
/// --staged` are refused in a checkout with `MERGE_HEAD` present, the
/// denial naming the merge and the checkout, and each is let through in
/// one without it; a leading `cd` into the merging checkout is refused
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

/// A refusal as it happened: from a checkout where another session's merge
/// stands, `allow-array.json`, a `cd` into a linked worktree joined by
/// `&&` to `P=(…)` and the commit, and `allow-git-c.json`'s `git -C` are
/// judged in the worktree alone and let through; `deny-array-then.json`'s
/// `;` after a `cd` into the worktree's `.git` file, which fails, runs the
/// commit where the line started, and is refused.
#[test]
fn an_array_assignment_after_a_cd_leaves_the_commit_in_the_worktree() {
    let manifest = Manifest::load(workspace()).unwrap();
    let merging = operating("operation-array");
    Repo::at(&merging).git(&["worktree", "add", "-q", "-b", "own", "own"]);
    let linked = merging.path().join("own");
    for (name, to) in [
        ("allow-array.json", linked.clone()),
        ("allow-git-c.json", linked.clone()),
        ("deny-array-then.json", linked.join(".git")),
    ] {
        let refused = operation_refuses(&manifest, merging.path(), name, "/var/empty/wt", &to);
        assert_eq!(refused, name.starts_with("deny"), "{name}");
    }
}

/// A refusal as it happened: from a checkout where another session's merge
/// stands, `allow-other-repository.json` `cd`s into another repository
/// and, past a `;`, stages and commits there by pathspec. The `cd` into a
/// directory it can enter does not fail, so the commit writes that
/// repository's index and not the merge's, and is let through, as is
/// `allow-git-c-other-repository.json`'s `-C` form; so are both into a
/// linked worktree of the merging checkout, whose index is its own. Both
/// moved into the merging checkout are refused, as the walks of
/// `rule_fixtures.rs` refuse `deny-own-checkout.json` and
/// `deny-git-c-own-checkout.json`, and so is a `cd` into a file there,
/// which fails and leaves the commit where the line started, as
/// the walks refuse `deny-or-then-cd.json`'s and `deny-and-then-cd.json`'s
/// `cd`, which may not run.
#[test]
fn a_commit_in_another_repository_is_judged_where_its_cd_or_c_moves_it() {
    let manifest = Manifest::load(workspace()).unwrap();
    let merging = operating("operation-elsewhere");
    let other = checkout("operation-elsewhere-other", None);
    Repo::at(&merging).git(&["worktree", "add", "-q", "-b", "own", "own"]);
    let linked = merging.path().join("own");
    let file = merging.path().join("seams.toml");
    let elsewhere = [
        "allow-other-repository.json",
        "allow-git-c-other-repository.json",
    ];
    for name in elsewhere {
        for (to, expected) in [
            (other.path(), false),
            (linked.as_path(), false),
            (merging.path(), true),
        ] {
            let refused = operation_refuses(&manifest, merging.path(), name, "/tmp", to);
            assert_eq!(refused, expected, "{name} in {}", to.display());
        }
    }
    let failed = operation_refuses(&manifest, merging.path(), elsewhere[0], "/tmp", &file);
    assert!(failed, "a cd into a file");
}

/// Whether `git.operation` refuses the fixture `name`, its line's `from`
/// spelled `to`, judged in `merging`.
fn operation_refuses(
    manifest: &Manifest,
    merging: &Path,
    name: &str,
    from: &str,
    to: &Path,
) -> bool {
    let mut fixture = call(&fixtures().join("rules/git.operation").join(name));
    let line = fixture.tool_input.command.as_deref();
    fixture.tool_input.command = line.map(|l| l.replace(from, &to.display().to_string()));
    let judged = judge(manifest, merging, &fixture).unwrap();
    judged.denial().is_some_and(|d| d.rule == "git.operation")
}

/// The placement ruled 2026-10-12, which `git.main-ff-only` holds since
/// `git.merge-commits-itself` moved to the merge worktree (ruled
/// 2026-10-30): a merge is refused in the main checkout and let through in
/// a linked worktree, where a builder merges `main` into its branch; a
/// neighbour in main does not place a merge `-C` runs in the worktree
/// there.
#[test]
fn a_merge_is_refused_in_main_alone_and_a_builders_goes_through() {
    let manifest = Manifest::load(workspace()).unwrap();
    let main = checkout("merge-commits-itself", None);
    Repo::at(&main).git(&["worktree", "add", "-q", "-b", "own", "own"]);
    let linked = main.path().join("own");
    let refused = |root: &Path, line: &str| {
        judge(&manifest, root, &bash(line))
            .unwrap()
            .findings
            .iter()
            .any(|f| f.rule.starts_with("git.m") && f.kind == Kind::Deny)
    };
    assert!(refused(main.path(), "git merge --no-ff t1234"));
    assert!(refused(main.path(), "git merge --ff-only t1234"));
    assert!(!refused(&linked, "git merge main"));
    let apart = format!(
        "git -C {} merge --abort; git -C {} merge main",
        main.path().display(),
        linked.display()
    );
    assert!(!refused(main.path(), &apart));
}

/// A denial names the checkout it judged: `git.operation`'s own
/// fixture, a commit by pathspec where another hand's merge stands, is
/// denied as judged in that checkout; the must-pass beside it, the same
/// commit run in a quiet checkout the line `cd`s into, is let through
/// though the shell starts where the merge stands.
#[test]
fn a_denial_names_the_checkout_it_judged_and_the_remedy() {
    let manifest = Manifest::load(workspace()).unwrap();
    let merging = operating("placed-merging");
    let quiet = checkout("placed-quiet", None);
    let commit = call(&fixtures().join("rules/git.operation/deny-commit.json"));
    let judged = judge(&manifest, merging.path(), &commit).unwrap();
    let denial = judged.denial().unwrap_or_else(|| panic!("{judged:?}"));
    let top = merging.path().canonicalize().unwrap();
    // The rule's remedy, then the checkout (ruled 2026-10-30).
    let remedy = manifest
        .rules
        .iter()
        .find(|r| r.id == "git.operation")
        .and_then(|r| r.remedy.clone());
    let remedy = remedy.expect("git.operation carries its remedy");
    assert!(
        denial
            .reason()
            .contains(&format!("Instead: {remedy}. Judged in {}.", top.display())),
        "{}",
        denial.reason()
    );
    let line = format!(
        "cd {} && git commit -q -F /var/empty/m -- a",
        quiet.path().display()
    );
    let judged = judge(&manifest, merging.path(), &bash(&line)).unwrap();
    assert!(judged.denial().is_none(), "{judged:?}");
}

/// A line permit cannot place says so and guesses no checkout:
/// past `cd "$UNSET_BY_THE_LINE"` a commit is denied under
/// `git.operation` where a merge stands in a checkout of the repository,
/// naming the command and the change it lost the line at; the must-pass,
/// the same line where no operation stands anywhere, is let through.
#[test]
fn a_line_permit_cannot_place_is_denied_saying_so_only_where_an_operation_stands() {
    let manifest = Manifest::load(workspace()).unwrap();
    let line = "cd \"$UNSET_BY_THE_LINE\" && git commit -q -F /var/empty/m -- a";
    let merging = operating("unplaced-merging");
    let judged = judge(&manifest, merging.path(), &bash(line)).unwrap();
    let denial = judged.denial().unwrap_or_else(|| panic!("{judged:?}"));
    assert_eq!(denial.rule, "git.operation");
    let reason = denial.reason();
    assert!(reason.contains("Not placed"), "{reason}");
    assert!(
        reason.contains("`git commit -q -F /var/empty/m -- a`"),
        "{reason}"
    );
    assert!(reason.contains("$UNSET_BY_THE_LINE"), "{reason}");
    assert!(!reason.contains("Judged in"), "{reason}");
    let quiet = checkout("unplaced-quiet", None);
    let judged = judge(&manifest, quiet.path(), &bash(line)).unwrap();
    assert!(judged.denial().is_none(), "{judged:?}");
}

/// The merge queue's rows (harness revision 183) as tracked:
/// `git.main-ff-only`, and `git.merge-commits-itself` on `where =
/// "staging"` as ruled 2026-10-30. Main's checkout refuses every merge but
/// the end of one, a fast-forward among them, since `worktree land` moves
/// main and no hand does (Round 5, Q6 (a)), and the merge worktree refuses
/// a merge that commits itself and lets one through that leaves its commit
/// to `git commit`. Each fixture of `git.main-ff-only` holds in main, and
/// each of `git.merge-commits-itself` in the merge worktree, its
/// `deny-after-cd.json` a `cd` into it.
#[test]
fn a_merge_is_refused_in_main_but_the_end_of_one_and_in_the_merge_worktree() {
    let tracked = Manifest::load(workspace()).unwrap();
    let within = |id: &str| tracked.rule(id).map(|r| r.within);
    assert_eq!(within("git.merge-commits-itself"), Some(Where::Staging));
    assert_eq!(within("git.main-ff-only"), Some(Where::Main));
    let (main, merge) = staging("merge-queue");
    let fired = |root: &Path, call: &Call, rule: &str| {
        judge(&tracked, root, call)
            .unwrap()
            .findings
            .iter()
            .any(|f| f.rule == rule && f.kind == Kind::Deny)
    };
    let in_main = |line: &str, rule: &str| fired(main.path(), &bash(line), rule);
    let in_merge = |line: &str, rule: &str| fired(&merge, &bash(line), rule);
    assert!(in_main(
        "git merge --no-ff --no-commit t1234",
        "git.main-ff-only"
    ));
    assert!(in_main("git merge --ff-only staging", "git.main-ff-only"));
    assert!(!in_main(
        "git merge --ff-only staging",
        "git.merge-commits-itself"
    ));
    assert!(!in_main(
        "git merge --no-ff t1234",
        "git.merge-commits-itself"
    ));
    assert!(in_merge(
        "git merge --no-ff t1234",
        "git.merge-commits-itself"
    ));
    assert!(!in_merge(
        "git merge --no-ff --no-commit t1234",
        "git.merge-commits-itself"
    ));
    assert!(!in_merge(
        "git merge --no-ff --no-commit t1234",
        "git.main-ff-only"
    ));

    for (rule, root) in [
        ("git.main-ff-only", main.path()),
        ("git.merge-commits-itself", merge.as_path()),
    ] {
        let dir = fixtures().join("rules").join(rule);
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            // A rule's replay corpus is the replay's to judge ([`super::replay`]).
            .filter(|n| Path::new(n).extension().is_some_and(|e| e == "json"))
            .collect();
        names.sort();
        assert!(names.len() >= 5, "{rule}: {names:?}");
        for name in &names {
            let denied = fired(root, &call(&dir.join(name)), rule);
            assert_eq!(denied, name.starts_with("deny"), "{rule}: {name}");
        }
    }
}
