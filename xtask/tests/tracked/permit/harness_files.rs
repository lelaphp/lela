//! The harness's own files: denied by their absolute path, a skill let
//! through beside the settings, the hook modules, and a shell write
//! judged where it runs.

use super::*;

#[test]
fn the_harness_files_are_denied_by_their_absolute_path() {
    let manifest = Manifest::load(workspace()).unwrap();
    for file in [
        "seams.toml",
        "docs/divergence.toml",
        "oracle.toml",
        ".claude/x/hooks.json",
    ] {
        for tool in ["Edit", "Write"] {
            let text = serde_json::json!({
                "tool_name": tool,
                "tool_input": {
                    "file_path": workspace().join(file).to_str().unwrap(),
                    "old_string": "a", "new_string": "b", "content": "b"
                }
            })
            .to_string();
            let judgement = judge(&manifest, workspace(), &Call::parse(&text).unwrap()).unwrap();
            let denial = judgement
                .denial()
                .unwrap_or_else(|| panic!("{tool} {file} allowed"));
            assert_eq!(denial.rule, "harness.self");
            assert!(
                denial.reason().contains("harness.self"),
                "{}",
                denial.reason()
            );
        }
    }
}

/// `harness.self`'s scope narrows to leave `.claude/skills/**` an ordinary
/// file, and what it leaves a person's is still denied: every
/// `deny*.json` under its fixtures is denied by the rule, the `Edit` and
/// the `Write` of `.claude/settings.json` among them, and every
/// `allow*.json` is let through, the `Edit` of a skill among them.
#[test]
fn harness_self_lets_a_skill_through_and_still_denies_the_settings() {
    let manifest = Manifest::load(workspace()).unwrap();
    let dir = fixtures().join("rules/harness.self");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("deny") || n.starts_with("allow"))
        .collect();
    names.sort();
    for named in [
        "allow-skill.json",
        "deny-settings-edit.json",
        "deny-settings-write.json",
    ] {
        assert!(names.iter().any(|n| n == named), "{named}: {names:?}");
    }
    for name in &names {
        let judged = judge(&manifest, workspace(), &call(&dir.join(name))).unwrap();
        let denied = judged.denial().map(|f| f.rule.as_str()) == Some("harness.self");
        assert_eq!(denied, name.starts_with("deny"), "{name}: {judged:?}");
        if !denied {
            assert!(
                judged.findings.iter().all(|f| f.rule != "harness.self"),
                "{name}: {judged:?}"
            );
        }
    }
}

/// Under the widened rows an `Edit`, a `Write` and the shell's writes of
/// a hook module are denied, each by its rule, and reading one, copying
/// it out and editing a file beside the modules are let through.
#[test]
fn the_hook_modules_are_a_persons_files_under_the_widened_rows() {
    let manifest = Manifest::load(workspace()).unwrap();
    let dir = fixtures().join("rules/harness.self/hook-modules");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for named in ["deny-edit.json", "deny-sed.json", "allow-cat.json"] {
        assert!(names.iter().any(|n| n == named), "{named}: {names:?}");
    }
    for name in &names {
        let call = call(&dir.join(name));
        let rule = if call.tool_name == "Bash" {
            "harness.self-shell"
        } else {
            "harness.self"
        };
        let judged = judge(&manifest, workspace(), &call).unwrap();
        if name.starts_with("deny") {
            assert_eq!(
                judged.denial().map(|f| f.rule.as_str()),
                Some(rule),
                "{name}: {judged:?}"
            );
        } else {
            assert!(judged.denial().is_none(), "{name}: {judged:?}");
        }
    }
}

/// The packages list under the scope #87 proposes (R13, spec
/// lelaphp/lela#32): an agent's `Edit` of `data/packages.toml` is denied by
/// `harness.self` once its scope names the list, which steer then writes
/// where a proposal adds a row, and let through by a scope that does not.
/// Once the tracked manifest names the list, both halves hold it.
#[test]
fn the_packages_list_is_denied_under_the_proposed_scope() {
    let tracked = Manifest::load(workspace()).unwrap();
    let mut proposed = tracked.clone();
    let rule = proposed.rules.iter_mut().find(|r| r.id == "harness.self");
    let rule = rule.unwrap();
    let mut scope: Vec<String> = rule.scope.iter().map(str::to_owned).collect();
    let listed = scope.iter().any(|s| s == "data/packages.toml");
    scope.push("data/packages.toml".to_owned());
    rule.scope = OneOrMany::Many(scope);
    let edit = call(&fixtures().join("rules/harness.self/packages-list/deny-edit.json"));
    for (manifest, denies) in [(&proposed, true), (&tracked, listed)] {
        let judged = judge(manifest, workspace(), &edit).unwrap();
        let rule = judged.denial().map(|f| f.rule.as_str());
        assert_eq!(rule == Some("harness.self"), denies, "{judged:?}");
    }
}

/// `harness.self-shell` under `where = "checkout"`, as ruled 2026-10-18: a
/// shell write to `docs/divergence.toml` is refused in a checkout, the
/// main one or a linked worktree, however the line reaches it, and let
/// through in a scratch repository outside every checkout, by either
/// spelling of a linked temporary directory, past a `cd` there that cannot
/// fail whatever joins it; a place `permit` cannot follow is read as a
/// checkout.
#[test]
fn harness_self_shell_judges_a_write_where_it_runs_and_only_in_a_checkout() {
    let mut manifest = Manifest::load(workspace()).unwrap();
    let rule = manifest
        .rules
        .iter_mut()
        .find(|r| r.id == "harness.self-shell");
    rule.unwrap().within = Where::Checkout;
    let main = checkout("self-shell-main", None);
    Repo::at(&main).git(&["worktree", "add", "-q", "-b", "own", "own"]);
    let (main, linked) = (main.path(), main.path().join("own"));
    let outside = Tree::new("permit-self-shell-outside").unwrap();
    Repo::init(&outside.join("repo"));
    let refused = |root: &Path, line: &str| {
        let judged = judge(&manifest, root, &bash(line)).unwrap();
        judged
            .denial()
            .is_some_and(|f| f.rule == "harness.self-shell")
    };
    let write = "echo '[[divergence]]' >> docs/divergence.toml";
    // The negative control, in each checkout.
    assert!(refused(main, write));
    assert!(refused(&linked, write));
    let (scratch, worktree) = (outside.path().display(), linked.display());
    let unlinked = outside.path().strip_prefix("/private").ok();
    let spellings = std::iter::once(scratch.to_string())
        .chain(unlinked.map(|path| format!("/{}", path.display())));
    for s in spellings {
        let line = format!("S={s} && cd \"$S/repo\" && {write}");
        assert!(!refused(main, &line), "{line}");
        assert!(!refused(&linked, &line), "{line}");
    }
    let piped = format!("cd {scratch}/repo && printf x 2>&1 | tee -a docs/divergence.toml");
    assert!(!refused(main, &piped), "{piped}");
    for line in [
        format!("cd {scratch}/repo; {write}"),
        format!("cd {scratch}/repo || true && {write}"),
        format!("cd {scratch}/repo\n{write}"),
    ] {
        assert!(!refused(main, &line), "{line}");
    }
    std::fs::create_dir(outside.join("links")).unwrap();
    std::os::unix::fs::symlink(main.join("seams.toml"), outside.join("links/seams.toml")).unwrap();
    std::fs::write(outside.join("file"), "").unwrap();
    let main_dir = main.display();
    for line in [
        // Into a checkout by an assigned variable, by a later `cd`, by a
        // subshell's, by git's `-C` and through a link.
        format!("W={worktree} && cd \"$W\" && {write}"),
        format!("cd {scratch}/repo && ls && cd {worktree} && {write}"),
        format!("cd {scratch}/repo && (cd {worktree} && {write})"),
        format!("cd {scratch}/repo && git -C {worktree} checkout -- seams.toml"),
        format!("cd {scratch}/links && sed -i '' 's/a/b/' seams.toml"),
        // Where `permit` cannot say: a variable the line did not assign,
        // `cd -`, a directory not on disk or no directory, a change that
        // may fail or run in a shell of its own before what follows it,
        // one a program runs unread, and a program's own directory.
        format!("cd \"$S/repo\" && {write}"),
        format!("cd {scratch}/repo && cd - && {write}"),
        format!("cd {scratch}/missing && {write}"),
        format!("cd {scratch}/file && {write}"),
        format!("cd {scratch}/file; {write}"),
        format!("cd {scratch}/missing || true && {write}"),
        format!("cd {scratch}/repo & {write}"),
        format!("cd {scratch}/repo | true && {write}"),
        format!("cd {scratch}/file\n{write}"),
        format!("cd {scratch}/repo && xargs sh -c 'cd {worktree} && {write}' < /dev/null"),
        format!("cd {scratch}/repo && env -C {worktree} sed -i '' 's/a/b/' seams.toml"),
        format!("cd {scratch}/repo && patch -d {worktree} seams.toml < fix.diff"),
        format!("cd {scratch}/repo && GIT_WORK_TREE={worktree} git checkout -- seams.toml"),
        format!("cd {scratch}/repo && ln -sf {main_dir}/seams.toml . && {write}"),
        format!("cd {scratch}/repo && env -iC {worktree} sed -i '' 's/a/b/' seams.toml"),
        format!("cd {scratch}/repo && patch -d{worktree} seams.toml < fix.diff"),
    ] {
        assert!(refused(main, &line), "{line}");
    }
}

/// A shell write past a `cd` to a variable the line did not assign is
/// denied saying permit could not place it, naming the command and the
/// change it lost the line at, and is never judged where the shell started;
/// the same write past a variable the line assigns from another it assigned
/// is placed where the value names.
#[test]
fn a_shell_write_permit_cannot_place_is_denied_saying_so() {
    let manifest = Manifest::load(workspace()).unwrap();
    let quiet = checkout("self-shell-unplaced", None);
    let write = "cat >> harness-plugins/harness/hooks/harness.test.ts <<'EOF'\n// a test\nEOF";
    let judged = judge(&manifest, quiet.path(), &bash(&format!("cd $R && {write}"))).unwrap();
    let denial = judged.denial().unwrap_or_else(|| panic!("{judged:?}"));
    assert_eq!(denial.rule, "harness.self-shell");
    let reason = denial.reason();
    assert!(reason.contains("Not placed"), "{reason}");
    assert!(
        reason.contains("`cat`") && reason.contains("`cd $R`"),
        "{reason}"
    );
    assert!(!reason.contains("Judged in"), "{reason}");
    let assigned = format!("S=/usr; R=$S/share; cd $R && {write}");
    let judged = judge(&manifest, quiet.path(), &bash(&assigned)).unwrap();
    assert!(judged.denial().is_none(), "{judged:?}");
}
