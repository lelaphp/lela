use super::*;

/// One fixture per rule permit judges: one call it fires on, one neighbour
/// it lets through (harness-design.md §4.1, §7).
#[test]
fn every_judged_rule_fires_on_its_fixture_and_not_on_its_neighbour() {
    let manifest = Manifest::load(workspace()).unwrap();
    let judged: Vec<_> = manifest
        .rules
        .iter()
        .filter(|r| judged_at_the_call(r))
        .collect();
    assert!(
        judged.len() >= 8,
        "{:?}",
        judged.iter().map(|r| &r.id).collect::<Vec<_>>()
    );
    // A rule scoped by `where` is judged in a checkout whose branch
    // records a ticket, which the tree the tests run in need not be.
    let ticketed = checkout("fixtures-ticketed", Some(1));
    // One scoped to `where = "operating"` in a checkout whose merge no
    // session of the fixture's started (#2476), and every other `Bash`
    // rule in a checkout where none stands, since the tests' own checkout
    // holds a merge while its merge commit's hook runs them; a write is
    // judged against the workspace's files.
    let merging = operating("fixtures-operating");
    let quiet = checkout("fixtures-quiet", None);
    for rule in judged {
        let dir = fixtures().join("rules").join(&rule.id);
        let kind = Kind::of(rule);
        let workspace = || match rule.within {
            Where::Anywhere if rule.tool.iter().any(|t| t == "Bash") => quiet.path(),
            Where::Anywhere => workspace(),
            Where::Ticketed => ticketed.path(),
            Where::Operating => merging.path(),
            Where::Main | Where::Checkout => quiet.path(),
        };
        // Every control of the rule's directory: each `deny*.json` (or
        // `note*.json`) must fire, each `allow*.json` must not (#625).
        let named = |prefix: &str| -> Vec<PathBuf> {
            let mut all: Vec<PathBuf> = std::fs::read_dir(&dir)
                .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
                .map(|entry| entry.unwrap().path())
                .filter(|p| {
                    let name = p.file_name().unwrap().to_str().unwrap();
                    name.starts_with(prefix) && p.extension().is_some_and(|e| e == "json")
                })
                .collect();
            all.sort();
            assert!(!all.is_empty(), "{}: no {prefix}*.json", dir.display());
            all
        };
        let firing = named(match kind {
            Kind::Deny => "deny",
            Kind::Note => "note",
        });
        for fires in &firing {
            let judgement = judge(&manifest, workspace(), &call(fires)).unwrap();
            let finding = judgement
                .findings
                .iter()
                .find(|f| f.rule == rule.id)
                .unwrap_or_else(|| {
                    panic!(
                        "{}: {} fired nothing: {judgement:?}",
                        rule.id,
                        fires.display()
                    )
                });
            assert_eq!(finding.kind, kind, "{}", rule.id);
            let reason = finding.reason();
            assert!(reason.contains(&rule.id), "{reason}");
            assert!(reason.contains(&rule.text), "{reason}");
            for adr in rule.adr.iter() {
                assert!(reason.contains(&format!("ADR-{adr}")), "{reason}");
            }
            if kind == Kind::Deny {
                assert_eq!(
                    judgement.denial().map(|f| f.rule.as_str()),
                    Some(rule.id.as_str()),
                    "{}",
                    fires.display()
                );
            }
        }
        // `allow-main.json` is a neighbour in main's checkout, where a
        // ticketed rule does not judge, an `allow-starter*.json` one
        // in a checkout whose operation the fixture's session started,
        // which a walk does not record, and an `allow-excused*.json` one
        // under an `unless_argv` a person has yet to paste (#3068): each
        // one's own test holds it.
        let neighbours = named("allow").into_iter().filter(|p| {
            let name = p.file_name().unwrap().to_string_lossy();
            !name.starts_with("allow-starter")
                && !unpasted(rule, &name)
                && (rule.within == Where::Anywhere || !p.ends_with("allow-main.json"))
        });
        for neighbour in neighbours {
            let judgement = judge(&manifest, workspace(), &call(&neighbour)).unwrap();
            assert!(
                judgement.findings.iter().all(|f| f.rule != rule.id),
                "{}: the neighbour {} fired: {judgement:?}",
                rule.id,
                neighbour.display()
            );
        }
    }
}

/// Every rule with a `tool` or a `scope` is either judged here, with its
/// fixture, or carries one of the five predicates that read the tree after
/// the write, or is read at the Stop alone (`tool = "Stop"`, #32), which
/// `permit` never judges and no after-the-write sensor reads; a fourth
/// kind cannot fall between the three.
#[test]
fn every_rule_with_a_tool_or_a_scope_is_judged_or_is_an_after_the_write_sensor() {
    let manifest = Manifest::load(workspace()).unwrap();
    for rule in manifest
        .rules
        .iter()
        .filter(|r| !r.tool.is_empty() || !r.scope.is_empty())
    {
        let p = &rule.predicate;
        let after_the_write = p.deny_methods.is_some()
            || p.check.is_some()
            || p.deny_lints.is_some()
            || p.avoid_from.is_some()
            || p.generated.is_some();
        if rule.tool.iter().any(|t| t == "Stop") {
            assert!(
                !judged_at_the_call(rule) && !after_the_write,
                "{}: a Stop rule is judged at the call {}, after the write {}",
                rule.id,
                judged_at_the_call(rule),
                after_the_write
            );
            continue;
        }
        assert!(
            judged_at_the_call(rule) != after_the_write,
            "{}: judged {}, after the write {}",
            rule.id,
            judged_at_the_call(rule),
            after_the_write
        );
    }
}

/// The fixture of the `xtask.lock-line` rule #55 proposes, judged under
/// the rule as its row would read, before a person commits it: the note
/// fires on a new `pub` item under `xtask/src` and not on its neighbour.
/// Once the tracked manifest carries the row, the walk above holds it and
/// this adds nothing.
#[test]
fn the_proposed_xtask_lock_line_rule_notes_its_fixture_and_not_its_neighbour() {
    let mut manifest = Manifest::load(workspace()).unwrap();
    if manifest.rule("xtask.lock-line").is_none() {
        let mut rule = Rule::named("xtask.lock-line", "xtask/src/**");
        rule.predicate.deny = None;
        rule.predicate.on = Some("pub".to_owned());
        manifest.rules.push(rule);
    }
    let dir = fixtures().join("rules/xtask.lock-line");
    let judged = judge(&manifest, workspace(), &call(&dir.join("note.json"))).unwrap();
    let finding = judged
        .findings
        .iter()
        .find(|f| f.rule == "xtask.lock-line")
        .unwrap_or_else(|| panic!("the note did not fire: {judged:?}"));
    assert_eq!(finding.kind, Kind::Note);
    assert!(
        judged.denial().is_none(),
        "a note is not a denial: {judged:?}"
    );
    let judged = judge(&manifest, workspace(), &call(&dir.join("allow.json"))).unwrap();
    assert!(
        judged.findings.iter().all(|f| f.rule != "xtask.lock-line"),
        "the neighbour fired: {judged:?}"
    );
}

/// The rule #33 proposes, `git.pathspec`, built through the constructors
/// as its row would read, so the predicate is proven before a person
/// commits the row: `require_argv = "--"` with `with = ["git add", "git
/// commit"]` denies a `git add` over the whole tree and a `git commit`
/// that names no path, with the rule's id and text in the denial, and
/// lets the pathspec forms and the hooks' own `git add -- <file>` through.
/// Once the tracked manifest carries the row, the walk above holds it and
/// this adds nothing.
#[test]
fn the_proposed_git_pathspec_rule_denies_its_fixture_and_not_its_neighbour() {
    let manifest = with_the_pathspec_rule(Manifest::load(workspace()).unwrap());
    let rule = manifest.rule("git.pathspec").unwrap();
    let dir = fixtures().join("rules/git.pathspec");
    let quiet = checkout("pathspec-quiet", None);
    let judged = judge(&manifest, quiet.path(), &call(&dir.join("deny.json"))).unwrap();
    let denial = judged
        .denial()
        .unwrap_or_else(|| panic!("the denial did not fire: {judged:?}"));
    assert_eq!(denial.rule, "git.pathspec");
    assert!(
        denial.reason().contains("git.pathspec"),
        "{}",
        denial.reason()
    );
    assert!(denial.reason().contains(&rule.text), "{}", denial.reason());
    let judged = judge(&manifest, quiet.path(), &call(&dir.join("allow.json"))).unwrap();
    assert!(
        judged.findings.iter().all(|f| f.rule != "git.pathspec"),
        "the neighbour fired: {judged:?}"
    );
    for line in [
        "git add .",
        "git add -u",
        "git commit -a -m msg",
        "git add -- .",
        "git -C /Users/x/dax-php commit -F /tmp/msg",
    ] {
        let judged = judge(&manifest, quiet.path(), &bash(line)).unwrap();
        assert_eq!(
            judged.denial().map(|f| f.rule.as_str()),
            Some("git.pathspec"),
            "{line}: {judged:?}"
        );
    }
    for line in [
        "git add -- AGENTS.md",
        "git commit -F /tmp/msg -- xtask/src",
        "git add -A -- xtask/fixtures/rules/git.pathspec",
        "git worktree add -b t/x .claude/worktrees/t/x main",
        "cargo add serde",
    ] {
        let judged = judge(&manifest, quiet.path(), &bash(line)).unwrap();
        assert!(judged.denial().is_none(), "{line}: {judged:?}");
    }
}

/// `lanes.nightly-flags`' fixtures under the row as proposed, in the
/// tracked manifest in place of the tracked row: every `deny*.json` denied
/// and every `allow*.json` let through. Under the row as tracked the same,
/// save that until the paste the `allow-excused*.json` lines, the tier's
/// one-subset reading, are denied with the rest (#3068).
#[test]
fn the_nightly_flags_let_the_tier_s_one_subset_reading_through() {
    let tracked = Manifest::load(workspace()).unwrap();
    let proposed = Tracked::workspace()
        .with_rule("lanes.nightly-flags", |rule| {
            *rule = PROPOSED_NIGHTLY_FLAGS.parse().unwrap();
        })
        .text();
    let proposed = Manifest::parse(&proposed).unwrap();
    let pasted = tracked
        .rule("lanes.nightly-flags")
        .is_some_and(|r| r.predicate.unless_argv.is_some());
    let quiet = checkout("nightly-flags-quiet", None);
    let dir = fixtures().join("rules/lanes.nightly-flags");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in [
        "allow-excused.json",
        "deny-jit-full.json",
        "deny-jit.json",
        "deny-bench.json",
    ] {
        assert!(names.iter().any(|n| n == name), "{name}: {names:?}");
    }
    let denied = |manifest: &Manifest, name: &str| {
        judge(manifest, quiet.path(), &call(&dir.join(name)))
            .unwrap()
            .findings
            .iter()
            .any(|f| f.rule == "lanes.nightly-flags")
    };
    for name in &names {
        let expect = name.starts_with("deny");
        assert_eq!(denied(&proposed, name), expect, "proposed row: {name}");
        let excused = name.starts_with("allow-excused");
        assert_eq!(
            denied(&tracked, name),
            expect || (excused && !pasted),
            "tracked row: {name}"
        );
    }
}

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

#[test]
fn a_rule_without_a_fixture_fails() {
    let manifest = Manifest::load(workspace()).unwrap();
    for rule in manifest.rules.iter().filter(|r| judged_at_the_call(r)) {
        let dir = fixtures().join("rules").join(&rule.id);
        assert!(dir.join("allow.json").is_file(), "{}", dir.display());
        assert!(
            dir.join("deny.json").is_file() || dir.join("note.json").is_file(),
            "{}",
            dir.display()
        );
    }
}

/// The `exempt` key (#23): a rule `permit` judges does not judge a call
/// whose file `exempt` covers, under the path grammar `scope` uses. The
/// fixture is the `witness.exit-authored` edit under the fence grammar's
/// own parser, `xtask/src/witness/fence.rs`, which the tracked row denies
/// until a person carries the exempt onto it; the row is built here
/// through the constructors so the predicate is proven without it, and
/// once the manifest carries it the first half holds it as tracked.
#[test]
fn a_call_the_rules_exempt_covers_is_not_judged_by_that_rule() {
    let manifest = Manifest::load(workspace()).unwrap();
    let dir = fixtures().join("rules/witness.exit-authored");
    let exempt = call(&dir.join("exempt.json"));
    let rule = manifest.rule("witness.exit-authored").unwrap();
    let judged = judge(&manifest, workspace(), &exempt).unwrap();
    if rule.exempts(Path::new("xtask/src/witness/fence.rs")) {
        assert!(
            judged.findings.iter().all(|f| f.rule != rule.id),
            "the tracked row exempts the parser: {judged:?}"
        );
    } else {
        assert_eq!(
            judged.denial().map(|f| f.rule.as_str()),
            Some(rule.id.as_str()),
            "the negative control: denied until the row lands: {judged:?}"
        );
    }

    let mut manifest = manifest;
    let rule = manifest
        .rules
        .iter_mut()
        .find(|r| r.id == "witness.exit-authored")
        .unwrap();
    if rule.exempt.is_empty() {
        rule.exempt = vec![
            "xtask/src/witness/**".to_owned(),
            "xtask/tests/witness.rs".to_owned(),
        ];
    }
    let judged = judge(&manifest, workspace(), &exempt).unwrap();
    assert!(
        judged
            .findings
            .iter()
            .all(|f| f.rule != "witness.exit-authored"),
        "the parser's own fence line is exempt: {judged:?}"
    );
    let judged = judge(&manifest, workspace(), &call(&dir.join("deny.json"))).unwrap();
    assert_eq!(
        judged.denial().map(|f| f.rule.as_str()),
        Some("witness.exit-authored"),
        "an extension stays denied: {judged:?}"
    );
}

/// Every predicate `permit` judges by a file reads `exempt` the same way:
/// the rule over `**` fires on a write at `crates/x.rs` and not on the
/// same write at `xtask/tests/x.rs`, which the exempt covers. `deny_argv`
/// judges a `Bash` call, which has no file, and an exempt names files, so
/// it is the one judged predicate an exempt cannot reach.
#[test]
fn every_judged_predicate_honours_exempt_under_the_scope_grammar() {
    let manifest = Manifest::load(workspace()).unwrap();
    let written = "pub fn read(p: *const u8) -> u8 {\n    unsafe { *p }\n}\nstatic M: std::sync::Mutex<u8> = todo!();\n";
    let probe = |shape: fn(&mut Rule)| {
        let mut rule = Rule::named("exempt.probe", "**");
        rule.exempt = vec!["xtask/tests/**".to_owned()];
        rule.predicate.deny = None;
        shape(&mut rule);
        rule
    };
    let rules = [
        probe(|r| r.predicate.deny = Some(true)),
        probe(|r| r.predicate.deny_pattern = Some("Mutex".to_owned())),
        probe(|r| r.predicate.deny_types = Some(vec!["std::sync::Mutex".to_owned()])),
        probe(|r| r.predicate.allow = Some(vec!["crates/php_heap/**".to_owned()])),
        probe(|r| r.predicate.on = Some("pub".to_owned())),
    ];
    for rule in rules {
        let name = rule.predicate.named().join(",");
        let mut manifest = manifest.clone();
        manifest.rules = vec![rule];
        let write = |path: &str| {
            let text = serde_json::json!({
                "tool_name": "Write",
                "tool_input": { "file_path": path, "content": written }
            })
            .to_string();
            judge(&manifest, workspace(), &Call::parse(&text).unwrap()).unwrap()
        };
        let fired = write("crates/x.rs");
        assert!(
            fired.findings.iter().any(|f| f.rule == "exempt.probe"),
            "{name}: the rule fires outside the exempt: {fired:?}"
        );
        let spared = write("xtask/tests/x.rs");
        assert!(
            spared.findings.is_empty(),
            "{name}: the rule fires inside the exempt: {spared:?}"
        );
    }
}

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
/// file (#626), and what it leaves a person's is still denied: every
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

#[test]
fn an_edit_inside_a_computed_region_body_is_denied_and_one_beside_it_allowed() {
    let manifest = Manifest::load(workspace()).unwrap();
    let dir = fixtures().join("regions");
    let inside = judge(&manifest, workspace(), &call(&dir.join("edit-body.json"))).unwrap();
    let denial = inside.denial().expect("an edit into the body is denied");
    assert_eq!(denial.rule, "computed.region-body");
    assert!(
        denial.reason().contains("computed run"),
        "{}",
        denial.reason()
    );

    let beside = judge(&manifest, workspace(), &call(&dir.join("edit-prose.json"))).unwrap();
    assert!(beside.denial().is_none(), "{beside:?}");

    let fenced = judge(&manifest, workspace(), &call(&dir.join("edit-fenced.json"))).unwrap();
    assert!(
        fenced.denial().is_none(),
        "markers inside a fence are prose: {fenced:?}"
    );

    let rewrite = judge(&manifest, workspace(), &call(&dir.join("write-body.json"))).unwrap();
    assert_eq!(
        rewrite.denial().map(|f| f.rule.as_str()),
        Some("computed.region-body"),
        "a Write that changes a body is the same edit"
    );
    let same = judge(&manifest, workspace(), &call(&dir.join("write-same.json"))).unwrap();
    assert!(same.denial().is_none(), "{same:?}");
    let opener = judge(
        &manifest,
        workspace(),
        &call(&dir.join("write-opener.json")),
    )
    .unwrap();
    assert!(
        opener.denial().is_none(),
        "a changed opener is a template change: {opener:?}"
    );
}

/// The two halves of the nightly lane the phpt runner asked for (#15):
/// the flag is denied wherever it stands in the call, not only beside the
/// subcommand, by `lanes.nightly-flags` (#1860), save the tier's `--jit`
/// lane on one subset `--subset` names (#3052), and upstream's runner is
/// denied where it is run and not where it is cited, by `lanes.nightly`.
/// The rule denied `--subset x --full` on neither count before, and denied
/// reading the file on the second.
#[test]
fn the_nightly_lane_is_denied_by_the_flag_and_not_by_a_citation() {
    let manifest = Manifest::load(workspace()).unwrap();
    let bash = |command: &str| {
        Call::parse(
            &serde_json::json!({
                "tool_name": "Bash",
                "tool_input": { "command": command, "description": "one call" }
            })
            .to_string(),
        )
        .unwrap()
    };
    let denied = |command: &str| {
        judge(&manifest, workspace(), &bash(command))
            .unwrap()
            .denial()
            .map(|f| f.rule.clone())
    };

    for (command, rule) in [
        (
            "cargo run -q -p xtask -- phpt --full",
            "lanes.nightly-flags",
        ),
        (
            "cargo run -p xtask -- phpt --subset Zend/tests --full",
            "lanes.nightly-flags",
        ),
        ("xtask phpt --jit force", "lanes.nightly-flags"),
        (
            "cargo run -q -p xtask -- phpt --jit exit-fuzz --full",
            "lanes.nightly-flags",
        ),
        ("php run-tests.php -p target/release/php", "lanes.nightly"),
        ("cd php-src && ./run-tests.php ext/pcre", "lanes.nightly"),
    ] {
        assert_eq!(
            denied(command).as_deref(),
            Some(rule),
            "the lane must be denied: {command}"
        );
    }

    for command in [
        "cargo run -q -p xtask -- phpt --subset pcre",
        "xtask phpt --subset Zend/tests --jit force",
        "cargo run -q -p xtask -- phpt --jit exit-fuzz --subset tests",
        "cargo test -p xtask --test phpt",
        "grep -n 'EXPECTF' /Users/x/php-src-8.5.9/run-tests.php",
        "echo 'upstream (run-tests.php:1885-1889) merges the streams'",
    ] {
        assert_eq!(
            denied(command),
            None,
            "the subset and a citation are not the lane: {command}"
        );
    }
}

/// The fixture is the quiet kind of stale and not the loud kind: it parses,
/// and the lane it lets through is the one the manifest on disk denies
/// (#15, #16). Nothing in the exit says so.
#[test]
fn the_stale_fixture_parses_and_lets_through_what_the_manifest_on_disk_denies() {
    let stale = Manifest::parse(&stale_manifest()).unwrap();
    let now = Manifest::load(workspace()).unwrap();
    let call = bash_call("cargo run -p xtask -- phpt --subset Zend/tests --full");
    assert_eq!(
        judge(&now, workspace(), &call)
            .unwrap()
            .denial()
            .map(|f| f.rule.clone())
            .as_deref(),
        Some("lanes.nightly-flags"),
        "the manifest on disk denies the lane"
    );
    assert!(
        judge(&stale, workspace(), &call)
            .unwrap()
            .denial()
            .is_none(),
        "and the stale one lets it through, exiting 0 as it does"
    );
}

/// `unsafe.homes` judges code, and the harness's own sensors carry the
/// keyword in their negative controls, which are strings and comments: the
/// audit's tests pin `unsafe_sites("unsafe { a(); }")`, and while that read
/// as a site the file could not be written to at all (#18).
#[test]
fn an_unsafe_inside_a_string_or_a_comment_is_not_a_site() {
    let manifest = Manifest::load(workspace()).unwrap();
    let denied = |content: &str| {
        let text = serde_json::json!({
            "tool_name": "Write",
            "tool_input": { "file_path": "crates/php_engine/src/executor.rs", "content": content },
        })
        .to_string();
        let call = Call::parse(&text).unwrap();
        judge(&manifest, workspace(), &call)
            .unwrap()
            .findings
            .iter()
            .any(|f| f.rule == "unsafe.homes")
    };

    assert!(
        denied("let x = unsafe { *p };"),
        "a site outside the homes unsafe.homes names is still denied"
    );
    assert!(denied("pub unsafe fn go() {}"));
    for allowed in [
        r#"let s = "unsafe { a(); }";"#,
        r##"let s = r#"unsafe fn b()"#;"##,
        "// unsafe { a(); }\nlet x = 1;",
        "/* unsafe fn b() */\nlet x = 1;",
        "let c = \'x\'; // a char literal is not a lifetime\nlet s = \"unsafe {\";",
    ] {
        assert!(!denied(allowed), "{allowed:?} carries no unsafe code");
    }

    // The file the ticket found: the audit's own source, unchanged.
    let audit = std::fs::read_to_string(workspace().join("xtask/src/unsafe_audit.rs")).unwrap();
    assert!(
        !denied(&audit),
        "the audit's own negative controls are strings, not sites"
    );
}

/// An unsafe attribute, `#[unsafe(no_mangle)]`, asserts an unsafe contract
/// as a block does, and is a site for both sensors of `unsafe.homes`: the
/// rule's fixture carries it outside a home and inside one (#46).
#[test]
fn an_unsafe_attribute_is_a_site_outside_a_home_and_not_a_finding_inside_one() {
    let manifest = Manifest::load(workspace()).unwrap();
    let dir = fixtures().join("rules/unsafe.homes");
    let fired = |name: &str| {
        judge(&manifest, workspace(), &call(&dir.join(name)))
            .unwrap()
            .findings
            .iter()
            .any(|f| f.rule == "unsafe.homes")
    };
    assert!(
        fired("deny-attribute.json"),
        "the attribute outside a home is denied at the edit"
    );
    assert!(
        !fired("allow-attribute.json"),
        "the attribute inside a home is allowed at the edit"
    );
}

/// The two rows #28 proposes for a person to add: a ticket closes through
/// `xtask ticket close` and by nothing else, so `gh issue close` and `gh
/// issue reopen` are denied by `deny_argv` with `with`, the shape
/// `git.no-verify` has, and the same act through `gh api` with a closing
/// or reopening state by a `deny_pattern`. Each fixture is held under the
/// row, as the tracked file carries it or as the constructors build it.
#[test]
fn the_proposed_ticket_close_rules_fire_on_their_fixtures_and_not_on_their_neighbours() {
    let manifest = with_the_ticket_close_rules(Manifest::load(workspace()).unwrap());
    for rule in ["ticket.close", "ticket.close-api"] {
        let dir = fixtures().join("rules").join(rule);
        let allowed = judge(&manifest, workspace(), &call(&dir.join("allow.json"))).unwrap();
        assert!(allowed.denial().is_none(), "{rule}: {allowed:?}");
        for fixture in [dir.join("deny.json"), dir.join("deny-reopen.json")] {
            let judged = judge(&manifest, workspace(), &call(&fixture)).unwrap();
            assert_eq!(
                judged.denial().map(|f| f.rule.as_str()),
                Some(rule),
                "{}: {judged:?}",
                fixture.display()
            );
        }
    }
    for line in [
        "gh api --method PATCH /repos/o/r/issues/28 --input /tmp/close.json",
        "gh api graphql -f query='mutation { closeIssue(input: {issueId: \"I_1\"}) { issue { id } } }'",
    ] {
        let judged = judge(&manifest, workspace(), &bash(line)).unwrap();
        assert_eq!(
            judged.denial().map(|f| f.rule.as_str()),
            Some("ticket.close-api"),
            "{line}: {judged:?}"
        );
    }
    for line in [
        "gh pr close 3",
        "cargo xtask ticket close 28",
        "gh api repos/o/r/issues?state=closed",
    ] {
        let judged = judge(&manifest, workspace(), &bash(line)).unwrap();
        assert!(judged.denial().is_none(), "{line}: {judged:?}");
    }
}

/// The whole-file case (#134, #510): the shape the ticket named, a `Write`
/// of a whole file with a status `--accept` captured in it, as one fixture
/// beside the `Edit` calls the directory carries. The fixture is judged
/// twice: over this workspace, where the file at that path carries no such
/// line, the write adds one and is denied with the rule id; over a tree
/// where the same file stands, it adds none and is permitted with no
/// reading behind it, which is the file a session must be able to write
/// again. Beside it, `beside-a-pinned-one.json` edits that same tree, the
/// verdicts untouched, and brings one more: the negative control of the
/// same pair, denied though the line it types stands in the file already
/// and the crate's reading is fresh.
#[test]
fn the_whole_file_fixture_is_denied_here_and_permitted_where_the_file_carries_it() {
    let dir = fixtures().join("rules/witness.exit-authored");
    let written = call(&dir.join("whole-file.json"));
    assert_eq!(
        written.tool_input.file_path.as_deref(),
        Some(FILE),
        "the case is over the crate the scratch half lays down"
    );
    let text = written
        .tool_input
        .content
        .clone()
        .expect("a Write carries the whole file");
    assert!(text.contains("exit=255"), "a captured status: {text}");

    let manifest = Manifest::load(workspace()).unwrap();
    let denied = denial(workspace(), &manifest, &written)
        .expect("the file at that path carries no such line");
    assert!(denied.contains(EXIT_AUTHORED), "{denied}");

    let root = scratch("unless-whole-file");
    let manifest = Manifest::load(&root).unwrap();
    lay_down(&root, &text);
    assert_eq!(
        denial(&root, &manifest, &written),
        None,
        "the whole file as it stands adds no verdict"
    );

    witness_reading(&root);
    let beside = call(&dir.join("beside-a-pinned-one.json"));
    let denied = denial(&root, &manifest, &beside)
        .expect("the edit brings a verdict the file does not carry yet");
    assert!(denied.contains(EXIT_AUTHORED), "{denied}");
}

/// End to end (#105): `xtask witness --accept` captures the oracle's
/// status onto the fence line and writes its reading, and `xtask permit
/// --diff` over that tree permits the line it wrote. Take the reading away
/// and the same tree is denied with the rule id.
#[test]
fn what_accept_wrote_permit_diff_permits_over_the_same_tree() {
    if !oracle_is_here() {
        return;
    }
    let root = scratch("unless-accept");
    lay_down(&root, &block("exit"));
    let readings = witness::run(
        &root,
        &Manifest::load(&root).unwrap(),
        &Registry::load(&root).unwrap(),
        &Ask {
            crates: vec![CRATE.to_owned()],
            accept: Accept::Status,
            ..Ask::default()
        },
    )
    .unwrap();
    Store::of_workspace(&root).record(&readings).unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join(FILE)).unwrap(),
        block("exit=3"),
        "the oracle's status is on the fence line and nothing else moved"
    );

    Repo::at(&root).git(&["add", "--", CRATE]);
    let out = xtask(&root, &["permit", "--diff"]).ok();
    assert!(!out.contains("deny "), "{out}");

    std::fs::remove_file(Store::of_workspace(&root).path("witness", CRATE).unwrap()).unwrap();
    let ran = xtask(&root, &["permit", "--diff"]);
    assert_eq!(ran.code(), 1, "{ran}");
    let out = ran.stdout;
    assert!(
        out.contains("deny ") && out.contains(EXIT_AUTHORED) && out.contains(FILE),
        "{out}"
    );
}

/// `git.operation` (#2476, as #2530 ruled it): `git add -- <path>`, a
/// commit by pathspec, another hand's `git merge --continue` and a `git
/// restore --staged` are refused in a checkout with `MERGE_HEAD` present,
/// the denial naming the merge and the checkout, and each is let through
/// in one without it; a leading `cd` into the merging checkout is refused
/// from anywhere, every subcommand the widened argv names is refused
/// there, and what reads the index is let through. The walks above hold
/// the fixtures where a merge stands; this adds the checkout without one.
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

/// `harness.self-shell` under `where = "checkout"`, as #2887 ruled and
/// #2915 built: a shell write to `docs/divergence.toml` is refused in a
/// checkout, the main one or a linked worktree, however the line reaches
/// it, and let through in a scratch repository outside every checkout,
/// by either spelling of a linked temporary directory; a place `permit`
/// cannot follow is read as a checkout.
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
    // The negative control #2887 names, in each checkout.
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
        format!("cd {scratch}/repo; {write}"),
        format!("cd {scratch}/repo || true && {write}"),
        format!("cd {scratch}/repo & {write}"),
        format!("cd {scratch}/repo | true && {write}"),
        format!("cd {scratch}/repo\n{write}"),
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

/// `unless_pattern` (#1149): a hit of the `deny_pattern` whose own command
/// also matches it is let through, and a hit in another command of the
/// same line is not.
#[test]
fn unless_pattern_allows_a_call_its_deny_pattern_matches() {
    let manifest = probe(|rule| rule.predicate.unless_pattern = Some("--subset".to_owned()));
    let root = workspace();
    assert!(denied(&manifest, root, "cargo xtask phpt --census"));
    assert!(!denied(
        &manifest,
        root,
        "cargo xtask phpt --subset ext/zlib"
    ));
    assert!(!denied(
        &manifest,
        root,
        "cargo xtask phpt --census --subset Zend/tests; echo ok"
    ));
    assert!(denied(
        &manifest,
        root,
        "cargo xtask phpt --subset ext/zlib && cargo xtask phpt"
    ));
    // The row that carries one without a `deny_pattern` is refused.
    let text = Tracked::workspace()
        .with_rule("orphan", |rule| {
            rule.extend(
                "tool = \"Bash\"\ndeny = true\nunless_pattern = \"x\"\ntext = \"t\"\n"
                    .parse::<toml::Table>()
                    .unwrap(),
            );
        })
        .text();
    let refused = Manifest::parse(&text).unwrap_err().to_string();
    assert!(
        refused.contains("`unless_pattern` beside `deny`"),
        "{refused}"
    );
}

/// `lanes.builder-subset`'s fixture (#1145, #1149), as `require_argv =
/// "--subset"` under `with = "xtask phpt"` reads it (#1860): in a
/// checkout whose branch records a ticket the lane without `--subset` is
/// denied, however xtask is run, and the subsets, an extension's and
/// Zend/tests, as a word or after `=`, pass; the merge check's call
/// passes in a checkout that records none, and is denied in one that
/// does, so it is `where` that lets it through.
#[test]
fn the_builder_subset_rule_denies_the_lane_in_a_ticketed_checkout_alone() {
    let manifest = Manifest::load(workspace()).unwrap();
    let dir = fixtures().join("rules/lanes.builder-subset");
    let ticketed = checkout("builder-subset-ticketed", Some(1145));
    let main = checkout("builder-subset-main", None);
    let rule = |root: &Path, file: &str| {
        let judged = judge(&manifest, root, &call(&dir.join(file))).unwrap();
        judged.denial().map(|f| f.rule.clone())
    };
    let denied = Some("lanes.builder-subset".to_owned());
    assert_eq!(rule(ticketed.path(), "deny.json"), denied);
    assert_eq!(rule(ticketed.path(), "allow.json"), None);
    assert_eq!(rule(ticketed.path(), "allow-zend.json"), None);
    assert_eq!(rule(main.path(), "allow-main.json"), None);
    assert_eq!(rule(ticketed.path(), "allow-main.json"), denied);
    // Through `cargo run` on the package or a variable the line assigns,
    // the lane is still run; the subset after `=` is still named.
    assert_eq!(rule(ticketed.path(), "deny-run.json"), denied);
    assert_eq!(rule(ticketed.path(), "deny-variable.json"), denied);
    assert_eq!(rule(ticketed.path(), "allow-equals.json"), None);
    // A message whose subject or body cites the instrument, under cargo
    // or not, runs nothing (#790, #1771).
    assert_eq!(rule(ticketed.path(), "allow-heredoc.json"), None);
    assert_eq!(rule(ticketed.path(), "allow-message-cargo.json"), None);
    // Reading the instrument's options runs no lane, and the exception is
    // the instrument's own flag, not a later `-h` (#1424).
    assert_eq!(rule(ticketed.path(), "allow-help.json"), None);
    assert_eq!(rule(ticketed.path(), "deny-help-elsewhere.json"), denied);
}

/// `lanes.nightly`'s heredoc fixtures (#790): a commit whose message has a
/// line opening `run-tests.php:2950)` cites upstream's runner and runs
/// nothing, so it passes; the runner first in the call, or after a
/// separator, is still denied, and so is its run under php.
#[test]
fn the_nightly_rule_denies_the_runner_where_it_is_run_and_not_a_message_line() {
    let manifest = Manifest::load(workspace()).unwrap();
    let dir = fixtures().join("rules/lanes.nightly");
    let quiet = checkout("nightly-quiet", None);
    let rule = |file: &str| {
        let judged = judge(&manifest, quiet.path(), &call(&dir.join(file))).unwrap();
        judged.denial().map(|f| f.rule.clone())
    };
    let denied = Some("lanes.nightly".to_owned());
    assert_eq!(rule("allow-heredoc.json"), None);
    assert_eq!(rule("deny-first.json"), denied);
    assert_eq!(rule("deny-separator.json"), denied);
    assert_eq!(rule("deny.json"), denied);
    // Flags before the runner, or a binary named by a variable, still run
    // it; reading its source does not (#1446).
    assert_eq!(rule("deny-flags.json"), denied);
    assert_eq!(rule("deny-variable.json"), denied);
    assert_eq!(rule("allow-read.json"), None);
}

/// A `Bash` rule judges what a command runs, not text quoted inside it
/// (#1771, #1832): a heredoc that cites the nightly lane's command, as a
/// ticket's draft or a program on python's stdin, passes
/// `lanes.nightly-flags`; the same command run after the heredoc closes,
/// or in a body a shell reads as the commands it runs, is denied.
#[test]
fn the_nightly_rule_judges_what_the_call_runs_and_not_a_heredoc_that_cites_it() {
    let manifest = Manifest::load(workspace()).unwrap();
    let dir = fixtures().join("rules/lanes.nightly-flags");
    let rule = |file: &str| {
        let judged = judge(&manifest, workspace(), &call(&dir.join(file))).unwrap();
        judged.denial().map(|f| f.rule.clone())
    };
    let denied = Some("lanes.nightly-flags".to_owned());
    assert_eq!(rule("allow-heredoc-lane.json"), None);
    assert_eq!(rule("allow-heredoc-python.json"), None);
    assert_eq!(rule("deny-after-heredoc.json"), denied);
    assert_eq!(rule("deny-shell-heredoc.json"), denied);
    assert_eq!(rule("deny-heredoc-piped.json"), denied);
}

/// Every `Bash` rule judged at the call holds to each of its fixtures
/// (#1832, #1860): every `deny*.json` is denied by the rule and every
/// `allow*.json` let through, whether the rule reads the line as a
/// `deny_pattern` or the command's argv as `deny_argv` or `require_argv`.
/// A rule scoped `where = "ticketed"` is judged in a checkout whose branch
/// records a ticket, but for an `allow-main*` call, the merge check's,
/// which passes in main's checkout alone. A rule with an `exempt_label`
/// asks the tracker, an `allow-starter*` call passes an operation its
/// session started, which a walk does not record, and an
/// `allow-excused*` call an `unless_argv` yet to be pasted: each is held
/// by its own test.
#[test]
fn every_bash_rule_holds_to_each_of_its_fixtures() {
    let manifest = Manifest::load(workspace()).unwrap();
    let ticketed = checkout("bash-fixtures-ticketed", Some(1832));
    let main = checkout("bash-fixtures-main", None);
    let merging = operating("bash-fixtures-operating");
    let rules: Vec<&Rule> = manifest
        .rules
        .iter()
        .filter(|r| {
            judged_at_the_call(r) && r.tool.iter().any(|t| t == "Bash") && r.exempt_label.is_empty()
        })
        .collect();
    // `git.no-verify`, `ticket.close-api`, `lanes.nightly`,
    // `lanes.nightly-flags`, `lanes.builder-subset` and `computed.force`
    // at #1877: a filter that finds fewer reads nothing.
    assert!(
        rules.len() >= 6,
        "{:?}",
        rules.iter().map(|r| &r.id).collect::<Vec<_>>()
    );
    for rule in rules {
        let dir = fixtures().join("rules").join(&rule.id);
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for name in names
            .iter()
            .filter(|n| !n.starts_with("allow-starter") && !unpasted(rule, n))
        {
            let root = match rule.within {
                Where::Ticketed if name.starts_with("allow-main") => main.path(),
                Where::Ticketed => ticketed.path(),
                Where::Anywhere => workspace(),
                Where::Operating => merging.path(),
                Where::Main | Where::Checkout => main.path(),
            };
            let judged = judge(&manifest, root, &call(&dir.join(name))).unwrap();
            let fired = judged
                .findings
                .iter()
                .any(|f| f.rule == rule.id && f.kind == Kind::Deny);
            assert_eq!(fired, name.starts_with("deny"), "{}: {name}", rule.id);
        }
    }
}
