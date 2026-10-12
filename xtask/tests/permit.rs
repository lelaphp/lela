//! `xtask permit`, the judge at the hook's call (harness-design.md §4.4).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use xtask::divergence::Registry;
use xtask::manifest::{Manifest, OneOrMany, Rule, Where};
use xtask::oracle::Promise;
use xtask::permit::{Call, Finding, Kind, judge, judged_at_the_call};
use xtask::reading::{Reading, Status, Store};
use xtask::scratch::Tree;
use xtask::witness::{self, Accept, Ask};

mod common;
use common::scratch_workspace;

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

fn fixtures() -> PathBuf {
    xtask::package_dir().join("fixtures")
}

fn call(path: &Path) -> Call {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    Call::parse(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

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
        // ticketed rule does not judge, and an `allow-starter*.json` one
        // in a checkout whose operation the fixture's session started,
        // which a walk does not record: each one's own test holds it.
        let neighbours = named("allow").into_iter().filter(|p| {
            let starter = p
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("allow-starter"));
            !starter && (rule.within == Where::Anywhere || !p.ends_with("allow-main.json"))
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

/// The manifest with the `git.pathspec` row #33 proposes, as the tracked
/// file carries it or as the constructors build it.
fn with_the_pathspec_rule(mut manifest: Manifest) -> Manifest {
    if manifest.rule("git.pathspec").is_none() {
        let mut rule = Rule::named("git.pathspec", "");
        rule.scope = OneOrMany::None;
        rule.tool = OneOrMany::One("Bash".to_owned());
        rule.predicate.deny = None;
        rule.predicate.require_argv = Some("--".to_owned());
        rule.predicate.with = OneOrMany::Many(vec!["git add".to_owned(), "git commit".to_owned()]);
        "An agent stages and commits by pathspec, `git add -- <paths>` and `git commit -F <file> -- <paths>`: the index is shared with every session in the checkout."
            .clone_into(&mut rule.text);
        manifest.rules.push(rule);
    }
    manifest
}

fn bash(line: &str) -> Call {
    Call::parse(
        &serde_json::json!({"tool_name": "Bash", "tool_input": {"command": line}}).to_string(),
    )
    .unwrap()
}

/// The `git.no-verify` row #1001 proposes: `with` names git's `commit` and
/// not the word, and the global option that points the hooks elsewhere is
/// denied beside `-n` and `--no-verify`.
const PROPOSED_NO_VERIFY: [&str; 4] = [
    "--no-verify",
    "-n",
    "-c core.hooksPath",
    "--config-env core.hooksPath",
];

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
    row.predicate.with = OneOrMany::One("git commit".to_owned());
    row.predicate.deny_argv = Some(PROPOSED_NO_VERIFY.map(str::to_owned).to_vec());
    let tracked_reads_hooks_path = tracked
        .rule("git.no-verify")
        .and_then(|r| r.predicate.deny_argv.as_ref())
        .is_some_and(|d| d.iter().any(|e| e == "-c core.hooksPath"));
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
        let tracked_expect = expect && (name != "deny-hooks-path.json" || tracked_reads_hooks_path);
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

/// The widening #2283 rules in: the function-hook modules,
/// `harness-plugins/hooks/*.ts`, join `harness.self`'s scope and each of
/// `harness.self-shell`'s five alternations, since the module hands
/// `permit` the evidence of the person's approval. A widened scope is a
/// changed value `steer` refuses, so a person lands it, or approves it in
/// the session; until then the two rows are widened here by the same two
/// substitutions as the diff put to the person, and once the tracked
/// manifest carries them the substitutions find nothing and the tracked
/// rows are held.
const HOOK_MODULES: [(&str, &str); 2] = [
    (
        r#""harness-plugins/hooks/hooks.json", "#,
        r#""harness-plugins/hooks/hooks.json", "harness-plugins/hooks/*.ts", "#,
    ),
    (
        r"harness-plugins/hooks/hooks\.json|",
        r#"harness-plugins/hooks/hooks\.json|harness-plugins/hooks/[^/\s'"|;&)<>]*\.ts|"#,
    ),
];

/// The tracked manifest with [`HOOK_MODULES`] applied, where it is not.
fn with_the_hook_modules() -> Manifest {
    let mut text = std::fs::read_to_string(workspace().join("seams.toml")).unwrap();
    if !text.contains("harness-plugins/hooks/*.ts") {
        let [(scope_was, scope), (shell_was, shell)] = HOOK_MODULES;
        assert_eq!(text.matches(scope_was).count(), 1, "harness.self's scope");
        assert_eq!(text.matches(shell_was).count(), 5, "harness.self-shell");
        text = text.replace(scope_was, scope).replace(shell_was, shell);
    }
    Manifest::parse(&text).unwrap()
}

/// Under the widened rows an `Edit`, a `Write` and the shell's writes of
/// a hook module are denied, each by its rule, and reading one, copying
/// it out and editing a file beside the modules are let through.
#[test]
fn the_hook_modules_are_a_persons_files_under_the_widened_rows() {
    let manifest = with_the_hook_modules();
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
    // The rows as tracked, before the widening lands, deny none of them:
    // what the fixtures prove is the widening and nothing beside it.
    let tracked = Manifest::load(workspace()).unwrap();
    if !std::fs::read_to_string(workspace().join("seams.toml"))
        .unwrap()
        .contains("harness-plugins/hooks/*.ts")
    {
        for name in names.iter().filter(|n| n.starts_with("deny")) {
            let judged = judge(&tracked, workspace(), &call(&dir.join(name))).unwrap();
            assert!(judged.denial().is_none(), "{name}: {judged:?}");
        }
    }
}

/// A worktree carries the tracked manifest, so it is a workspace of its
/// own by the one rule (`Call::workspace`, #57); it also has a `.git`, so
/// it is the checkout its readings go to. This one is gitignored.
fn lay_worktree(worktree: &Path) {
    std::fs::create_dir_all(worktree).unwrap();
    std::fs::write(worktree.join(".git"), "gitdir: nowhere\n").unwrap();
    std::fs::copy(workspace().join("seams.toml"), worktree.join("seams.toml")).unwrap();
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

/// A worktree laid down beside the checkout, where `worktree add` lays
/// one (revision 56), is a checkout of the repository outside the root:
/// a file in it is judged against that worktree's manifest, and is
/// neither outside every workspace nor read against the checkout's. Git's
/// own `.git` says which repository a checkout is of, so a clone laid
/// beside it with a manifest of its own is another repository's, and its
/// files stay outside the workspace as before.
#[test]
fn a_file_in_a_worktree_beside_the_checkout_is_judged_against_that_worktree() {
    let tree = Tree::new("permit-beside").unwrap();
    let root = tree.join("repo");
    scratch_workspace(workspace(), &root).unwrap();
    let beside = tree.join("repo-worktrees/t-x/beside");
    sh(
        &root,
        &[
            "git",
            "worktree",
            "add",
            "-q",
            "-b",
            "t-x/beside",
            beside.to_str().unwrap(),
        ],
    );
    let marker = "edited by a person, says the worktree's manifest";
    let manifest = std::fs::read_to_string(beside.join("seams.toml")).unwrap();
    let manifest = manifest.replacen("are edited by a person.", &format!("are {marker}."), 1);
    assert!(
        manifest.contains(marker),
        "the harness.self text is rewritten"
    );
    std::fs::write(beside.join("seams.toml"), manifest).unwrap();

    let edit = |file: &Path| {
        Call::parse(
            &serde_json::json!({"tool_name": "Edit", "cwd": root.to_str().unwrap(),
                "tool_input": {"file_path": file.to_str().unwrap(),
                    "old_string": "a", "new_string": "b"}})
            .to_string(),
        )
        .unwrap()
    };
    let call = edit(&beside.join("oracle.toml"));
    let judged_in = call.workspace(&root);
    assert_eq!(judged_in, beside, "the worktree beside is the workspace");
    assert_eq!(call.path(&judged_in).unwrap(), Path::new("oracle.toml"));
    let judged = judge(&Manifest::load(&judged_in).unwrap(), &judged_in, &call).unwrap();
    let denial = judged
        .denial()
        .expect("the pin is a person's in the worktree too");
    assert_eq!(denial.rule, "harness.self");
    assert!(denial.reason().contains(marker), "{}", denial.reason());

    // A Bash call is placed by its cwd, there as under the root.
    let shell = Call::parse(
        &serde_json::json!({"tool_name": "Bash", "cwd": beside.to_str().unwrap(),
            "tool_input": {"command": "cargo test -p xtask"}})
        .to_string(),
    )
    .unwrap();
    assert_eq!(shell.workspace(&root), beside);
    // And a file places a call by itself, wherever the shell stands: an
    // edit in the checkout from a session in the worktree is the
    // checkout's, and one in the worktree asked of the worktree is the
    // worktree's own (#1047).
    let across = Call::parse(
        &serde_json::json!({"tool_name": "Edit", "cwd": beside.to_str().unwrap(),
            "tool_input": {"file_path": root.join("oracle.toml").to_str().unwrap(),
                "old_string": "a", "new_string": "b"}})
        .to_string(),
    )
    .unwrap();
    assert_eq!(across.workspace(&root), root.canonicalize().unwrap());
    let within = edit(&beside.join("oracle.toml"));
    assert_eq!(
        within.workspace(&beside),
        beside.canonicalize().unwrap(),
        "not the checkout the session stands in"
    );

    // A clone beside is another repository: its files are no workspace's.
    let clone = tree.join("repo-t-y");
    sh(
        &tree,
        &[
            "git",
            "clone",
            "-q",
            root.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    let call = edit(&clone.join("oracle.toml"));
    assert_eq!(call.workspace(&root), root);
    assert_eq!(call.path(&root), None);
}

/// A worktree beside a scratch checkout, on a branch of its own: the
/// place `worktree add` lays one (revision 56).
fn worktree_beside(tree: &Path, root: &Path, branch: &str) -> PathBuf {
    let beside = tree.join("repo-worktrees").join(branch);
    sh(
        root,
        &[
            "git",
            "worktree",
            "add",
            "-q",
            "-b",
            branch,
            beside.to_str().unwrap(),
        ],
    );
    beside.canonicalize().unwrap()
}

/// The hook's stdin for one call, placed by `cwd` when it has no file.
fn hook_stdin(tool: &str, cwd: &Path, input: &serde_json::Value) -> String {
    serde_json::json!({"session_id": "3f1c0d2a-9b7e-4c55-8f1a-2d6e7b9c0a11",
        "tool_name": tool, "cwd": cwd.to_str().unwrap(), "tool_input": input})
    .to_string()
}

/// A worktree laid before a `seams.toml` change carries rules no build of
/// the checkout is compiled against, and each tree is judged by the rules
/// it carries: the checkout's build hands a call there to the worktree's
/// own, which judges it against its own tree, and returns its decision
/// (#1047). A rule only the worktree's manifest carries fires on a write
/// there and not on the same write in the checkout; a Bash call is placed
/// by its working directory the same way, and a row the worktree words
/// differently is denied in the worktree's words; the readings are the
/// worktree's build's, in its store. The walk in the worktree passes over
/// the checkout's build seeded into its `target/debug`, which refuses a
/// manifest it was not compiled against, and `cargo run` there builds the
/// harness under test against the worktree's manifest.
#[test]
fn a_call_in_a_worktree_whose_manifest_differs_is_judged_by_that_worktrees_build() {
    let tree = Tree::new("permit-hand-off").unwrap();
    let root = tree.join("repo");
    xtask::scratch::repository(workspace(), &root).unwrap();
    let beside = worktree_beside(&tree, &root, "t-x/handed");
    let marker = "not available to agents, says the worktree's manifest";
    let manifest = std::fs::read_to_string(beside.join("seams.toml")).unwrap();
    let manifest = manifest.replacen("not available to agents.", &format!("{marker}."), 1);
    assert!(
        manifest.contains(marker),
        "the git.no-verify text is rewritten"
    );
    std::fs::write(
        beside.join("seams.toml"),
        format!(
            "{manifest}\n[[rule]]\nid = \"worktree.only\"\ntool = [\"Edit\", \"Write\"]\n\
             scope = \"only-here/**\"\ndeny = true\n\
             text = \"A file under only-here/ is refused by the worktree's manifest alone.\"\n"
        ),
    )
    .unwrap();
    // The checkout's dependency build, seeded the way `worktree add` seeds
    // it; where the volume cannot clone, the build there starts cold.
    std::fs::create_dir_all(beside.join("target")).unwrap();
    Command::new("cp")
        .args(["-c", "-R"])
        .arg(workspace().join("target/debug"))
        .arg(beside.join("target/debug"))
        .output()
        .ok();

    let write = |file: &Path| {
        hook_stdin(
            "Write",
            &root,
            &serde_json::json!({"file_path": file.to_str().unwrap(), "content": "one\n"}),
        )
    };
    let (code, out, err) = permit(&root, &write(&beside.join("only-here/one.txt")), &[]);
    assert_eq!(code, 0, "the worktree's build judged the call: {err}");
    assert!(out.contains("worktree.only"), "{out}{err}");
    assert!(out.contains("\"deny\""), "{out}");
    let reading = Store::of_workspace(&beside)
        .read("permit", "worktree.only")
        .unwrap()
        .expect("the worktree's build recorded its denial in its own store");
    assert_eq!(reading.status, Status::Fail);
    assert!(
        Store::of_workspace(&root)
            .read("permit", "permit.unjudged-call")
            .unwrap()
            .is_none()
            && Store::of_workspace(&beside)
                .read("permit", "permit.unjudged-call")
                .unwrap()
                .is_none(),
        "nothing went unjudged"
    );

    // The same write in the checkout is the checkout's to judge, by rules
    // that carry no such row.
    let (code, out, err) = permit(&root, &write(&root.join("only-here/one.txt")), &[]);
    assert_eq!(code, 0, "{err}");
    assert!(!out.contains("worktree.only"), "{out}");
    assert!(!out.contains("\"deny\""), "{out}");

    // A Bash call is placed by its working directory: in the worktree it
    // is denied in the worktree's words, in the checkout in the checkout's.
    let commit = |cwd: &Path| {
        hook_stdin(
            "Bash",
            cwd,
            &serde_json::json!({"command": "git commit --no-verify -m x"}),
        )
    };
    let (code, out, err) = permit(&root, &commit(&beside), &[]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains(marker), "the worktree's rule: {out}{err}");
    let (code, out, err) = permit(&root, &commit(&root), &["--above-the-command-hook"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("git.no-verify"), "{out}");
    assert!(!out.contains(marker), "the checkout's rule: {out}");
}

/// A hand-off that does not run decides nothing, and nothing is allowed on
/// its silence (#1047): the worktree here has no build to hand the call
/// to, so the checkout's build exits without judging, as it did before
/// there was a hand-off, and the call reaches the end of the hook's walk
/// unjudged. Every candidate of that walk hands the call to the same
/// worktree, so it is the failure no rebuild repairs, in the worktree's
/// store, naming the worktree's manifest.
#[test]
fn a_hand_off_that_does_not_run_leaves_the_call_unjudged() {
    let tree = Tree::new("permit-hand-off-fails").unwrap();
    let root = tree.join("repo");
    scratch_workspace(workspace(), &root).unwrap();
    let beside = worktree_beside(&tree, &root, "t-x/unbuilt");
    let manifest = std::fs::read_to_string(beside.join("seams.toml")).unwrap();
    std::fs::write(
        beside.join("seams.toml"),
        format!("{manifest}\n# a row a person has since settled\n"),
    )
    .unwrap();
    let stdin = hook_stdin(
        "Write",
        &root,
        &serde_json::json!({"file_path": beside.join("README.md").to_str().unwrap(),
            "content": "one\n"}),
    );
    let (code, out, err) = permit(&root, &stdin, &[]);
    assert_ne!(code, 0, "the call was not judged: {out}");
    assert!(out.trim().is_empty(), "and nothing was decided: {out}");
    assert!(
        err.contains(&beside.join("seams.toml").display().to_string()),
        "{err}"
    );
    let reading = Store::of_workspace(&beside)
        .read("permit", "permit.unjudged-call")
        .unwrap()
        .expect("the refusal was recorded");
    assert_eq!(reading.status, Status::Fail);
    assert!(
        Store::of_workspace(&beside)
            .read("permit", "permit.stale-build")
            .unwrap()
            .is_none(),
        "no rebuild of the checkout judges it"
    );
}

/// The unsafe attribute written into a file of the `unsafe.homes` fixture.
fn edit_of(file: &Path) -> Call {
    Call::parse(
        &serde_json::json!({"tool_name": "Edit", "cwd": workspace().to_str().unwrap(),
            "tool_input": {"file_path": file.to_str().unwrap(), "old_string": "}\n",
                "new_string": "}\n\n#[unsafe(no_mangle)]\npub extern \"C\" fn go() {}\n"}})
        .to_string(),
    )
    .unwrap()
}

/// The `ext.no-locks` row #1781 ruled, in the fixture that carries it word
/// for word (#1888): permit denies the once type and the thread-local,
/// each a write into an extension, and lets the neighbour through, whose
/// prose names `thread_local` without invoking it. The thread-local is
/// denied by `deny_macros` alone: under the same row without it, the type
/// ban lets the write through. Once the tracked manifest carries the row,
/// `ext.no-locks`' own fixture walk holds the rule as it holds it today.
#[test]
fn the_ruled_no_locks_row_denies_a_once_type_and_a_thread_local_and_not_their_neighbour() {
    let root = fixtures().join("clippy/once-and-thread-local");
    let manifest = Manifest::load(&root).unwrap();
    let denied = |manifest: &Manifest, file: &str| {
        judge(manifest, &root, &call(&root.join(file)))
            .unwrap()
            .denial()
            .map(|f| f.rule.clone())
    };
    for file in ["once.json", "thread-local.json"] {
        assert_eq!(
            denied(&manifest, file).as_deref(),
            Some("ext.no-locks"),
            "{file}"
        );
    }
    assert_eq!(denied(&manifest, "allow.json"), None);

    let mut types_alone = manifest.clone();
    for rule in &mut types_alone.rules {
        rule.predicate.deny_macros = None;
    }
    assert_eq!(denied(&types_alone, "thread-local.json"), None);
    assert_eq!(
        denied(&types_alone, "once.json").as_deref(),
        Some("ext.no-locks")
    );
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

/// A fixture without a manifest of its own is judged by the workspace's,
/// as before #57: its directory is under the workspace and nothing
/// between them carries a `seams.toml`.
#[test]
fn a_fixture_without_a_manifest_is_judged_by_the_workspaces() {
    let file = fixtures().join("rules/ext.no-locks/deny.json");
    assert!(
        !file.parent().unwrap().join("seams.toml").exists(),
        "the fixture carries no manifest"
    );
    let call = edit_of(&file);
    assert_eq!(
        call.workspace(workspace()),
        workspace().canonicalize().unwrap()
    );
    assert_eq!(
        call.path(workspace()).unwrap(),
        Path::new("xtask/fixtures/rules/ext.no-locks/deny.json")
    );
}

/// `permit --diff` and `done` apply the same resolution to a diff that
/// touches a fixture source (#57): the diff judge resolves each file's
/// workspace as the hook does, so a source in a home of the fixture's
/// manifest passes and one outside it is refused with the fixture's text.
#[test]
fn diff_judges_a_fixture_source_by_the_fixture_manifest() {
    let root = scratch("diff-fixture");
    let fixture = "xtask/fixtures/rules/unsafe.homes";
    let manifest = std::fs::read_to_string(fixtures().join("rules/unsafe.homes/seams.toml"))
        .unwrap()
        .replace(
            "pin = \"../../../../oracle.toml\"",
            &format!("pin = \"{}\"", workspace().join("oracle.toml").display()),
        );
    write(&root, &format!("{fixture}/seams.toml"), &manifest);
    write(
        &root,
        &format!("{fixture}/ext/pcre/Cargo.toml"),
        "[package]\nname = \"pcre\"\n",
    );
    let site = "pub fn read(p: *const u8) -> u8 {\n    unsafe { *p }\n}\n";
    write(&root, &format!("{fixture}/ext/pcre/src/ffi/mod.rs"), site);
    sh(&root, &["git", "add", "-A"]);
    let (code, out, err) = permit(&root, "", &["--diff"]);
    assert_eq!(code, 0, "a home of the fixture's manifest: {out}{err}");

    write(
        &root,
        &format!("{fixture}/crates/php_unit/src/lib.rs"),
        site,
    );
    sh(&root, &["git", "add", "-A"]);
    let (code, out, _) = permit(&root, "", &["--diff"]);
    assert_eq!(code, 1, "outside the fixture's homes: {out}");
    assert!(
        out.contains(&format!("deny {fixture}/crates/php_unit/src/lib.rs"))
            && out.contains("(fixture)"),
        "the path is the checkout's and the text the fixture's: {out}"
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

/// The label a rule does not judge (#341, harness-design.md ledger 52).
/// `ticket.close` denies `gh issue close` so that nothing closes a ticket
/// but the instrument that read its acceptance criteria; a chart sheet
/// carries none, so the row names `frontier:chart` and the close of a
/// filled sheet is not the act the rule is for. The label is the
/// tracker's fact, so permit asks the tracker, here a recorded `gh` first
/// on `PATH` and never the network: the sheet's close is let through, the
/// same call on a ticket carrying no such label is denied as before, and
/// a tracker that will not answer leaves the denial standing.
#[test]
fn a_close_is_denied_unless_the_tracker_says_the_issue_carries_the_exempt_label() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch("exempt-label");
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let issue = |number: u64, labels: &str| {
        serde_json::json!({
            "number": number,
            "title": format!("issue {number}"),
            "body": "",
            "state": "OPEN",
            "url": format!("https://example.invalid/issues/{number}"),
            "labels": labels
                .split_whitespace()
                .map(|name| serde_json::json!({ "name": name }))
                .collect::<Vec<_>>(),
            "blocking": { "nodes": [] },
        })
        .to_string()
    };
    std::fs::write(
        bin.join("460.json"),
        issue(460, "frontier:chart track:t-opus-5"),
    )
    .unwrap();
    std::fs::write(bin.join("28.json"), issue(28, "ready-for-agent")).unwrap();
    let gh = bin.join("gh");
    std::fs::write(
        &gh,
        format!(
            "#!/bin/sh\nif [ \"$1\" = issue ] && [ \"$2\" = view ] && [ -f '{0}/'\"$3\"'.json' ]; then cat '{0}/'\"$3\"'.json'; exit 0; fi\nexit 1\n",
            bin.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();

    let judged = |command: &str| -> Option<String> {
        let stdin = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": command, "description": "close" }
        })
        .to_string();
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut child = Command::new(xtask::scratch::xtask(env!("CARGO_BIN_EXE_xtask")))
            .arg("--root")
            .arg(&*root)
            .arg("permit")
            .env("PATH", path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(&mut child.stdin.take().unwrap(), stdin.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(0), "permit decides with its JSON");
        let stdout = String::from_utf8(out.stdout).unwrap();
        let text = stdout.trim();
        if text.is_empty() {
            return None;
        }
        let got: serde_json::Value = serde_json::from_str(text).unwrap();
        Some(
            got["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        )
    };

    assert_eq!(
        judged("gh issue close 460 --comment 'sheet 7 continues this'"),
        None,
        "the sheet carries the label the rule does not judge"
    );
    let ticket = judged("gh issue close 28 --comment 'done'").expect("a ticket is denied");
    assert!(ticket.contains("ticket.close"), "{ticket}");
    let unknown = judged("gh issue close 9001").expect("an issue the tracker will not answer for");
    assert!(unknown.contains("ticket.close"), "{unknown}");
}

fn permit(root: &Path, stdin: &str, args: &[&str]) -> (i32, String, String) {
    let mut child = Command::new(xtask::scratch::xtask(env!("CARGO_BIN_EXE_xtask")))
        .arg("--root")
        .arg(root)
        .arg("permit")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), stdin.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

/// A scratch workspace that is a git repository with the tracked manifest
/// and pin, so a denial there is stamped and stored without touching this
/// workspace's store.
fn scratch(name: &str) -> Tree {
    let dir = Tree::new(&format!("permit-{name}")).unwrap();
    scratch_workspace(workspace(), &dir).unwrap();
    dir
}

/// The stdin shapes recorded on Claude Code 2.1.263 (data/pre-day-1.md §4)
/// produce the recorded stdout shapes.
#[test]
fn the_recorded_stdin_shapes_produce_the_recorded_stdout_shapes() {
    let root = scratch("shapes");
    let dir = fixtures().join("hooks");
    let stdin = std::fs::read_to_string(dir.join("pretooluse-allow.json")).unwrap();
    let (code, out, err) = permit(&root, &stdin, &[]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.trim().is_empty(),
        "an allow with nothing to say prints nothing: {out}"
    );

    let stdin = std::fs::read_to_string(dir.join("pretooluse-deny.json")).unwrap();
    let (code, out, err) = permit(&root, &stdin, &[]);
    assert_eq!(code, 0, "{err}");
    let got: serde_json::Value = serde_json::from_str(&out).unwrap();
    let want: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("pretooluse-deny.stdout.json")).unwrap(),
    )
    .unwrap();
    let head = &got["hookSpecificOutput"];
    assert_eq!(
        head["hookEventName"],
        want["hookSpecificOutput"]["hookEventName"]
    );
    assert_eq!(
        head["permissionDecision"],
        want["hookSpecificOutput"]["permissionDecision"]
    );
    let reason = head["permissionDecisionReason"].as_str().unwrap();
    assert!(reason.contains("harness.self"), "{reason}");
    assert!(reason.contains("edited by a person"), "{reason}");
    assert_eq!(
        got.as_object().unwrap().len(),
        1,
        "the deny is the recorded shape and nothing beside it: {out}"
    );
    assert_eq!(head.as_object().unwrap().len(), 3, "{out}");
}

#[test]
fn a_denial_is_a_reading_with_instrument_permit_and_the_rule_id() {
    let root = scratch("reading");
    let stdin = std::fs::read_to_string(fixtures().join("hooks/pretooluse-deny.json")).unwrap();
    let (code, _, err) = permit(&root, &stdin, &[]);
    assert_eq!(code, 0, "{err}");
    let reading = Store::of_workspace(&root)
        .read("permit", "harness.self")
        .unwrap()
        .expect("the denial was recorded");
    assert_eq!(reading.instrument, "permit");
    assert_eq!(reading.rule, "harness.self");
    assert_eq!(reading.status, Status::Fail);
    assert!(
        reading.message.contains("seams.toml"),
        "{}",
        reading.message
    );
    assert!(reading.message.starts_with("Edit "), "{}", reading.message);
    assert_eq!(reading.key().len(), 40);
}

#[test]
fn a_note_rides_on_an_allow_as_additional_context_and_is_a_reading() {
    let root = scratch("note");
    let stdin = std::fs::read_to_string(fixtures().join("rules/api.lock-line/note.json")).unwrap();
    let (code, out, err) = permit(&root, &stdin, &[]);
    assert_eq!(code, 0, "{err}");
    let got: serde_json::Value = serde_json::from_str(&out).unwrap();
    let head = &got["hookSpecificOutput"];
    assert_eq!(head["hookEventName"], "PreToolUse");
    assert!(
        head.get("permissionDecision").is_none(),
        "a note does not decide: {out}"
    );
    let context = head["additionalContext"].as_str().unwrap();
    assert!(context.contains("api.lock-line"), "{context}");
    let reading = Store::of_workspace(&root)
        .read("permit", "api.lock-line")
        .unwrap()
        .expect("the note was recorded");
    assert_eq!(reading.status, Status::Pass);
}

/// The function-hook adapter judges a call before the command hook does
/// (harness-design.md §4.4): its denial ends the call and is recorded
/// here, while an allow goes on to the command hook, which records the
/// notes, so a note is one line of the log and not two.
#[test]
fn above_the_command_hook_a_denial_is_recorded_and_a_note_is_left_to_it() {
    let root = scratch("above");
    let dir = fixtures();
    let stdin = std::fs::read_to_string(dir.join("rules/api.lock-line/note.json")).unwrap();
    let (code, out, err) = permit(&root, &stdin, &["--above-the-command-hook"]);
    assert_eq!(code, 0, "{err}");
    let got: serde_json::Value = serde_json::from_str(&out).unwrap();
    let context = got["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(context.contains("api.lock-line"), "{context}");
    let store = Store::of_workspace(&root);
    assert!(
        store.read("permit", "api.lock-line").unwrap().is_none(),
        "the note is the command hook's to record"
    );

    let stdin = std::fs::read_to_string(dir.join("hooks/pretooluse-deny.json")).unwrap();
    let (code, out, err) = permit(&root, &stdin, &["--above-the-command-hook"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("\"deny\""), "{out}");
    let reading = store
        .read("permit", "harness.self")
        .unwrap()
        .expect("the denial ends the call here and is recorded here");
    assert_eq!(reading.status, Status::Fail);
}

/// #2734 as it happened: an agent's merge stands in main's checkout, its
/// starter recorded, and its line aborting it there and starting a merge
/// in its own worktree is judged by both adapters, the function hook's
/// first. That admission dropped the record, as the abort's own, and
/// recorded the worktree's start, before the command hook beneath judged
/// the same call; that one found the merge standing with no record,
/// refused the abort, and every later abort of the starter's with it. The
/// operation is recorded by the command hook alone, the adapter that
/// judges a call last, so the abort is the starter's under both.
#[test]
fn an_abort_the_function_hook_admits_is_its_starter_s_under_the_command_hook_too() {
    let root = scratch("operation-adapters");
    let worktree = root.join("own");
    let git = |dir: &Path, args: &[&str]| {
        let out = Command::new("git").args(args).current_dir(dir).output();
        assert!(out.unwrap().status.success(), "git {}", args.join(" "));
    };
    git(&root, &["worktree", "add", "-q", "-b", "own", "own"]);
    let line = |command: &str| {
        serde_json::json!({
            "session_id": "s1",
            "agent_id": "comp",
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": command},
        })
        .to_string()
    };
    let admitted = |stdin: &str, args: &[&str], what: &str| {
        let (code, out, err) = permit(&root, stdin, args);
        assert_eq!(code, 0, "{err}");
        assert!(!out.contains("\"deny\""), "{what} {args:?}: {out}");
    };
    let above: &[&str] = &["--above-the-command-hook"];
    let start = line("git merge --no-ff --no-commit topic");
    for adapter in [above, &[]] {
        admitted(&start, adapter, "the start");
    }
    std::fs::write(root.join(".git/MERGE_HEAD"), "0".repeat(40) + "\n").unwrap();
    let abort = line(&format!(
        "C={}; git -C $C merge --abort; git -C $C status --short | head -3; \
         git -C $C log --oneline -1; W={}; git -C $W status --short | head; \
         git -C $W merge main 2>&1 | tail -3",
        root.display(),
        worktree.display()
    ));
    for adapter in [above, &[]] {
        admitted(&abort, adapter, "the starter's abort");
    }
}

/// The adapter hands the call to permit on stdin, which no argument-length
/// limit bounds (harness-design.md §13 D.17): a Write of several megabytes
/// is judged whole.
#[test]
fn a_large_write_on_stdin_is_judged_whole() {
    let root = scratch("large");
    let content = format!(
        "{}\nstatic X: std::sync::Mutex<u8> = todo!();\n",
        "// ".repeat(1 << 20)
    );
    assert!(content.len() > 3_000_000);
    let call = serde_json::json!({
        "session_id": "s", "cwd": root.path(), "hook_event_name": "PreToolUse",
        "tool_name": "Write",
        "tool_input": {"file_path": "ext/pcre/src/cache.rs", "content": content},
    });
    let (code, out, err) = permit(&root, &call.to_string(), &[]);
    assert_eq!(code, 0, "{err}");
    let reason = serde_json::from_str::<serde_json::Value>(&out).unwrap()["hookSpecificOutput"]
        ["permissionDecisionReason"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        reason.contains("ext.no-locks"),
        "the lock at the end of the file was read: {reason}"
    );
}

#[test]
fn a_call_that_cannot_be_read_is_denied_not_dropped() {
    let root = scratch("garbage");
    let (code, out, _) = permit(&root, "not json", &[]);
    assert_eq!(code, 0);
    let got: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(got["hookSpecificOutput"]["permissionDecision"], "deny");
}

#[test]
fn bench_measures_p99_against_the_bar_and_writes_a_reading() {
    let root = scratch("bench");
    let (code, out, err) = permit(&root, "", &["--bench", "--runs", "6"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("p99"), "{out}");
    assert!(out.contains("100 ms"), "the bar is named: {out}");
    // The judge's cost is a reading over the harness crate, whose sources
    // the judge is; the store's key is the manifest's scope.
    let reading = Store::of_workspace(&root)
        .read("permit", "xtask")
        .unwrap()
        .expect("the bench is a reading");
    assert_eq!(reading.numbers["runs"], 6.into());
    assert_eq!(reading.key().len(), 40, "stamped over the harness crate");
    assert!(
        reading.numbers.contains_key("p99_ms"),
        "{:?}",
        reading.numbers
    );
    assert!(
        Store::of_workspace(&root)
            .read("permit", "harness.self")
            .unwrap()
            .is_none(),
        "the bench's own denials do not land in the store it reports to"
    );
}

/// The bench is a measurement, which owes no exit (harness-design.md §4.4, #61):
/// with the bar missed the exit is 0, the reading records `fail` and the
/// numbers, and the verdict is printed for a person to read.
#[test]
fn bench_with_the_bar_missed_exits_zero_and_records_fail() {
    let root = scratch("bench-missed");
    let (code, out, err) = permit(&root, "", &["--bench", "--runs", "3", "--bar", "0"]);
    assert_eq!(code, 0, "a measurement's exit is not its status: {err}");
    assert!(out.contains("bar p99 ≤ 0 ms: fail"), "{out}");
    let reading = Store::of_workspace(&root)
        .read("permit", "xtask")
        .unwrap()
        .expect("the bench is a reading");
    assert_eq!(reading.status, Status::Fail);
    assert_eq!(reading.numbers["runs"], 3.into());
    assert_eq!(reading.numbers["bar_ms"], 0.0.into());
    assert!(
        reading.numbers["p99_ms"].as_f64() > 0.0,
        "{:?}",
        reading.numbers
    );
}

fn sh(root: &Path, args: &[&str]) {
    let out = Command::new(args[0])
        .args(&args[1..])
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "x")
        .env("GIT_AUTHOR_EMAIL", "x@x")
        .env("GIT_COMMITTER_NAME", "x")
        .env("GIT_COMMITTER_EMAIL", "x@x")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write(root: &Path, path: &str, text: &str) {
    let file = root.join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, text).unwrap();
}

/// `permit --diff`, pre-commit's caller (harness-design.md §4.4, §6): the
/// staged files are judged as the `Write` of their staged content, a
/// denial refuses the commit with the rule id, and the harness's own
/// files, which a person commits, are a note and not a refusal.
#[test]
fn diff_judges_the_staged_files_and_refuses_a_denied_one_with_the_rule_id() {
    let root = scratch("diff");
    let (code, out, err) = permit(&root, "", &["--diff"]);
    assert_eq!(code, 0, "nothing staged is nothing denied: {out}{err}");

    let locked = "use std::sync::Mutex;\n\npub struct Cache(Mutex<Vec<u8>>);\n";
    write(&root, "ext/pcre/src/cache.rs", locked);
    sh(&root, &["git", "add", "ext/pcre/src/cache.rs"]);
    let (code, out, _) = permit(&root, "", &["--diff"]);
    assert_eq!(code, 1, "a denied staged file refuses the commit: {out}");
    assert!(
        out.contains("ext.no-locks"),
        "the rule id is printed: {out}"
    );
    assert!(out.contains("ext/pcre/src/cache.rs"), "{out}");
    let reading = Store::of_workspace(&root)
        .read("permit", "ext.no-locks")
        .unwrap()
        .expect("the denial was recorded");
    assert_eq!(reading.status, Status::Fail);
    assert!(
        reading.message.starts_with("Write ext/pcre/src/cache.rs"),
        "{}",
        reading.message
    );

    // What is judged is the index, not the working tree.
    write(
        &root,
        "ext/pcre/src/cache.rs",
        "pub struct Cache(Vec<u8>);\n",
    );
    let (code, out, _) = permit(&root, "", &["--diff"]);
    assert_eq!(code, 1, "the staged content still holds the lock: {out}");
    sh(&root, &["git", "add", "ext/pcre/src/cache.rs"]);
    let (code, out, err) = permit(&root, "", &["--diff"]);
    assert_eq!(code, 0, "{out}{err}");

    // The harness's own file: a note, since the hook cannot tell a person
    // from an agent, and the commit goes through.
    let manifest = std::fs::read_to_string(root.join("seams.toml")).unwrap();
    write(
        &root,
        "seams.toml",
        &format!("{manifest}\n# a person's note\n"),
    );
    sh(&root, &["git", "add", "seams.toml"]);
    let (code, out, err) = permit(&root, "", &["--diff"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("note seams.toml"), "{out}");
    assert!(out.contains("harness.self"), "{out}");
}

/// The hook's stdin carries `session_id`; a reading carries it, so the
/// `permits` section can count denials per session (harness-design.md
/// §4.6, §10 rule 6).
#[test]
fn a_denial_carries_the_session_of_the_call() {
    let root = scratch("session");
    let stdin = std::fs::read_to_string(fixtures().join("hooks/pretooluse-deny.json")).unwrap();
    let (code, _, err) = permit(&root, &stdin, &[]);
    assert_eq!(code, 0, "{err}");
    let reading = Store::of_workspace(&root)
        .read("permit", "harness.self")
        .unwrap()
        .expect("the denial was recorded");
    assert_eq!(reading.session, "3f1c0d2a-9b7e-4c55-8f1a-2d6e7b9c0a11");

    let (code, _, err) = permit(
        &root,
        "",
        &[
            "--json",
            r#"{"tool_name":"Edit","tool_input":{"file_path":"seams.toml","old_string":"a","new_string":"b"}}"#,
        ],
    );
    assert_eq!(code, 0, "{err}");
    let log = Store::of_workspace(&root).history("permit").unwrap();
    assert_eq!(
        log.readings.last().unwrap().session,
        "",
        "a call without a session is recorded without one"
    );
}

// ----------------------------------------------- the wiring at the tool call

/// The `PreToolUse` entries hooks.json declares, each with the tools its
/// matcher names.
fn pre_tool_use_entries() -> Vec<(Vec<String>, serde_json::Value)> {
    let text =
        std::fs::read_to_string(workspace().join("harness-plugins/hooks/hooks.json")).unwrap();
    let hooks: serde_json::Value = serde_json::from_str(&text).unwrap();
    let entries = hooks["hooks"]["PreToolUse"]
        .as_array()
        .unwrap_or_else(|| panic!("no PreToolUse entry: {hooks}"))
        .clone();
    entries
        .into_iter()
        .map(|entry| {
            let tools = entry["matcher"]
                .as_str()
                .unwrap_or_else(|| panic!("an entry without a matcher: {entry}"))
                .split('|')
                .map(str::to_owned)
                .collect();
            (tools, entry)
        })
        .collect()
}

/// The one command hooks.json declares for one `tool_name` at `PreToolUse`: the
/// line the hook runs before every call of it. Two entries walk the same
/// candidates and differ in what they do when none judges, so the `tool_name`
/// picks the line (#16 items 2 and 5).
fn pre_tool_use_command(tool: &str) -> String {
    let commands: Vec<String> = pre_tool_use_entries()
        .into_iter()
        .filter(|(tools, _)| tools.iter().any(|t| t == tool))
        .flat_map(|(_, entry)| entry["hooks"].as_array().cloned().unwrap_or_default())
        .map(|hook| hook["command"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        commands.len(),
        1,
        "one command judges a {tool} call: {commands:?}"
    );
    commands.into_iter().next().unwrap()
}

/// `PreToolUse` hands the call to permit, over the three tools the design
/// names, from the workspace root (harness-design.md §4.4). Each of them is
/// named by exactly one entry, so no call is judged twice by the command
/// hook and none is left to no entry at all.
#[test]
fn pre_tool_use_judges_the_three_tools_through_permit() {
    for tool in JUDGED {
        let named: Vec<_> = pre_tool_use_entries()
            .into_iter()
            .filter(|(tools, _)| tools.iter().any(|t| t == tool))
            .collect();
        assert_eq!(named.len(), 1, "{tool} is named by one entry: {named:?}");
        let command = pre_tool_use_command(tool);
        assert!(command.contains("cd \"$CLAUDE_PROJECT_DIR\""), "{command}");
        assert!(command.contains("permit"), "{command}");
    }
}

/// The tools permit judges, the matchers' union (harness-design.md §4.4).
const JUDGED: [&str; 3] = ["Edit", "Write", "Bash"];

/// The candidate that closes the list rebuilds rather than trusting a stale
/// build, so the hook's budget has to cover a build and not only a judging:
/// a warm recompile of xtask measured 4.88s when the fallback was written.
/// A budget too small for it times the hook out, and a call that times out
/// is a call that passed unjudged, which is the window the fallback closed
/// (harness-design.md §4.4). Every entry, since each walks that candidate.
#[test]
fn the_hook_leaves_room_for_the_rebuild_its_last_candidate_may_do() {
    for (tools, entry) in pre_tool_use_entries() {
        let timeout = entry["hooks"][0]["timeout"]
            .as_i64()
            .unwrap_or_else(|| panic!("{tools:?} declares no timeout"));
        assert!(
            timeout >= 60,
            "the cargo candidate needs room to rebuild: {tools:?} {timeout}"
        );
    }
}

/// The two adapters do not resolve the built binaries in one order (#16
/// item 4): the command hook takes the release build first and the module
/// beside it the debug one, so one stale build no longer silences both
/// layers at once, which is how an hour of calls went unjudged. The
/// candidate that closes each list is the same rebuild, so a tree in which
/// every build is stale still ends in one place.
#[test]
fn the_two_adapters_do_not_walk_the_built_binaries_in_one_order() {
    let command = pre_tool_use_command("Edit");
    let walk: Vec<&str> = command
        .split_once("for x in ")
        .unwrap_or_else(|| panic!("no walk in {command}"))
        .1
        .split_once("; do")
        .unwrap_or_else(|| panic!("no walk in {command}"))
        .0
        .split_whitespace()
        .collect();
    let module =
        std::fs::read_to_string(workspace().join("harness-plugins/hooks/harness.ts")).unwrap();
    let listed = module
        .split_once("const BUILT = [")
        .expect("the module lists the builds it walks")
        .1
        .split_once(']')
        .expect("a list that closes")
        .0;
    let order: Vec<&str> = listed
        .split(',')
        .map(|path| path.trim().trim_matches('\''))
        .filter(|path| !path.is_empty())
        .collect();
    assert_eq!(walk.len(), 2, "the command hook walks two builds: {walk:?}");
    assert_eq!(order.len(), 2, "and the module the same two: {order:?}");
    let mut by_name = (walk.clone(), order.clone());
    by_name.0.sort_unstable();
    by_name.1.sort_unstable();
    assert_eq!(by_name.0, by_name.1, "the same builds");
    assert_ne!(walk, order, "in orders that do not fall together");
}

/// What a build does when the command runs it: one that stands judges, and
/// one older than the manifest or the flag it is handed exits without
/// judging, the way a build from before `[telemetry]` landed did.
#[cfg(unix)]
#[derive(Clone, Copy)]
enum Build {
    Judges,
    DidNotJudge,
}

/// Lay a build down under the scratch workspace. One that judges prints its
/// name and the call it read, so what the command hands each candidate is
/// measured beside which candidate ran.
#[cfg(unix)]
fn build(root: &Path, path: &str, who: &str, how: Build) {
    use std::os::unix::fs::PermissionsExt;
    let text = match how {
        Build::Judges => format!("#!/bin/sh\nin=$(cat)\nprintf 'JUDGED={who} SAW=%s' \"$in\"\n"),
        Build::DidNotJudge => {
            format!("#!/bin/sh\ncat >/dev/null\necho '{who}: unknown field' >&2\nexit 1\n")
        }
    };
    write(root, path, &text);
    std::fs::set_permissions(root.join(path), std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Run the declared command as the hook does: the call on stdin, the
/// workspace in `CLAUDE_PROJECT_DIR`, and the scratch `bin` ahead of PATH so
/// the cargo the command closes with is the one laid down here.
#[cfg(unix)]
fn pre_tool_use(root: &Path, tool: &str, stdin: &str) -> (i32, String, String) {
    let path = format!(
        "{}:{}",
        root.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(pre_tool_use_command(tool))
        .current_dir(root)
        .env("CLAUDE_PROJECT_DIR", root)
        .env("PATH", path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), stdin.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

/// permit's exit is 0 whatever it decides, so a non-zero exit is a build
/// that never judged: the command walks its candidates until one does, and
/// cargo closes the list, rather than the call passing on a stale build's
/// silence (harness-design.md §4.4). The two adapters resolve the binary in
/// the same order, so a build that stood for both left the call unjudged
/// beneath a hook that had already failed open above it. The walk is one
/// walk, whichever entry declares it, so both are held to it.
#[cfg(unix)]
#[test]
fn a_build_that_did_not_judge_hands_the_call_to_the_next_candidate() {
    let call = std::fs::read_to_string(fixtures().join("hooks/pretooluse-deny.json")).unwrap();
    let call = call.trim();
    for tool in JUDGED {
        the_walk_is_the_same_for(tool, call);
    }
}

#[cfg(unix)]
fn the_walk_is_the_same_for(tool: &str, call: &str) {
    // The release build stands: it judges, and no other candidate runs.
    let root = scratch(&format!("wiring-stands-{tool}"));
    build(&root, "target/release/xtask", "release", Build::Judges);
    build(&root, "target/debug/xtask", "debug", Build::Judges);
    build(&root, "bin/cargo", "cargo", Build::Judges);
    let (code, out, err) = pre_tool_use(&root, tool, call);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("JUDGED=release"), "{out}");
    assert!(!out.contains("JUDGED=debug"), "one build judges: {out}");
    assert!(!out.contains("JUDGED=cargo"), "one build judges: {out}");
    assert!(out.contains(call), "the call reaches the build that judges");

    // The release build is older than the manifest: the next one judges.
    let root = scratch(&format!("wiring-stale-{tool}"));
    build(&root, "target/release/xtask", "release", Build::DidNotJudge);
    build(&root, "target/debug/xtask", "debug", Build::Judges);
    build(&root, "bin/cargo", "cargo", Build::Judges);
    let (code, out, err) = pre_tool_use(&root, tool, call);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("JUDGED=debug"), "{out} / {err}");
    assert!(!out.contains("JUDGED=cargo"), "{out}");
    assert!(
        out.contains(call),
        "the call the first build read reaches the second"
    );

    // No build stands: cargo, which rebuilds rather than trusting one.
    let root = scratch(&format!("wiring-cargo-{tool}"));
    build(&root, "target/release/xtask", "release", Build::DidNotJudge);
    build(&root, "target/debug/xtask", "debug", Build::DidNotJudge);
    build(&root, "bin/cargo", "cargo", Build::Judges);
    let (code, out, err) = pre_tool_use(&root, tool, call);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("JUDGED=cargo"), "{out} / {err}");
    assert!(out.contains(call), "the call reaches cargo");
}

/// Where the two entries part (#16 items 2 and 5): with every candidate
/// refusing, an Edit or a Write is denied, since a write no judge saw does
/// not proceed, and a Bash call is handed on, since an agent repairs a
/// broken harness with Bash and denying all three deadlocks the repair. A
/// deny is exit 0 with the JSON (D.16); the hand-on is the non-zero exit
/// the host reports as a non-blocking error, and the call proceeds.
#[cfg(unix)]
#[test]
fn with_no_candidate_judging_a_write_is_denied_and_a_bash_call_is_handed_on() {
    let call = std::fs::read_to_string(fixtures().join("hooks/pretooluse-deny.json")).unwrap();
    let call = call.trim();
    for tool in JUDGED {
        let root = scratch(&format!("wiring-none-{tool}"));
        build(&root, "target/release/xtask", "release", Build::DidNotJudge);
        build(&root, "target/debug/xtask", "debug", Build::DidNotJudge);
        build(&root, "bin/cargo", "cargo", Build::DidNotJudge);
        let (code, out, err) = pre_tool_use(&root, tool, call);
        if tool == "Bash" {
            assert_ne!(code, 0, "a Bash call is handed on: {out}");
            assert!(out.trim().is_empty(), "with no decision of its own: {out}");
            continue;
        }
        assert_eq!(code, 0, "a deny is exit 0 with the JSON: {err}");
        let got: serde_json::Value = serde_json::from_str(out.trim())
            .unwrap_or_else(|e| panic!("{tool}: {e}: {out} / {err}"));
        let head = &got["hookSpecificOutput"];
        assert_eq!(head["hookEventName"], "PreToolUse");
        assert_eq!(head["permissionDecision"], "deny");
        let reason = head["permissionDecisionReason"].as_str().expect(&out);
        assert!(reason.contains("did not judge"), "{reason}");
        assert!(
            reason.contains("cargo build -p xtask"),
            "the repair: {reason}"
        );
    }
}

/// The two halves of the nightly lane the phpt runner asked for (#15):
/// the flag is denied wherever it stands in the call, not only beside the
/// subcommand, by `lanes.nightly-flags` (#1860), and upstream's runner is
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
        (
            "xtask phpt --subset Zend/tests --jit force",
            "lanes.nightly-flags",
        ),
        (
            "cargo run -q -p xtask -- phpt --jit exit-fuzz --subset tests",
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

// ------------------------------ the build against the manifest it judges by

/// The manifest an earlier build was compiled against (#16): it parses
/// under today's grammar and judges the same call differently, which an
/// exit code cannot see.
fn stale_manifest() -> String {
    std::fs::read_to_string(fixtures().join("stale/seams.toml")).unwrap()
}

/// A scratch workspace holding that manifest: a build meeting rules it was
/// not compiled against.
fn scratch_with_a_stale_manifest(name: &str) -> Tree {
    let root = scratch(name);
    std::fs::write(root.join("seams.toml"), stale_manifest()).unwrap();
    root
}

fn bash_call(command: &str) -> Call {
    Call::parse(
        &serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": command, "description": "one call" }
        })
        .to_string(),
    )
    .unwrap()
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

/// permit's exit is 0 whatever it decides, so the refusal is the one thing
/// it says with an exit: a build that would judge by rules it was not
/// compiled against decides nothing, names both digests, and leaves the
/// call to the next candidate (#16, harness-design.md §4.4). The call this
/// one reads is denied by both manifests, and the refusal still stands: a
/// decision that happens to agree is not a decision this build may make.
#[test]
fn a_build_the_manifest_moved_under_refuses_to_judge_rather_than_judging_stale() {
    let root = scratch_with_a_stale_manifest("stale");
    let stdin = std::fs::read_to_string(fixtures().join("hooks/pretooluse-deny.json")).unwrap();
    let (code, out, err) = permit(&root, &stdin, &[]);
    assert_ne!(code, 0, "the build that cannot judge exits non-zero: {out}");
    assert!(out.trim().is_empty(), "and decides nothing: {out}");
    assert!(err.contains("seams.toml"), "{err}");
    // The digest compiled in, sha1 as `build.rs` prints it: a 40-digit
    // hex word beside the manifest's, which the refusal names both of.
    assert!(
        err.split(|c: char| !c.is_ascii_hexdigit())
            .filter(|w| w.len() == 40)
            .count()
            >= 2,
        "both digests are named: {err}"
    );
}

/// The refusal is a reading, so the store says what the judge did with the
/// call where the facts already are; through the outage of #16 nothing
/// said it. A build apart from the tree hands the call to the candidate
/// that rebuilds, which judges it, so the reading is a skip that names
/// where the call went and not a failure (#118): the store of the main
/// checkout held 5687 such failures against 23 denials, and the one number
/// a person wants was under them.
#[test]
fn the_refusal_that_hands_the_call_on_is_a_skip_that_names_the_next_candidate() {
    let root = scratch_with_a_stale_manifest("stale-reading");
    let stdin = std::fs::read_to_string(fixtures().join("hooks/pretooluse-deny.json")).unwrap();
    let (code, _, err) = permit(&root, &stdin, &[]);
    assert_ne!(code, 0, "{err}");
    let reading = Store::of_workspace(&root)
        .read("permit", "permit.stale-build")
        .unwrap()
        .expect("the refusal was recorded");
    assert_eq!(reading.instrument, "permit");
    assert_eq!(reading.rule, "permit.stale-build");
    assert_eq!(
        reading.status,
        Status::Skip,
        "a call that went on was judged: {}",
        reading.message
    );
    assert_eq!(reading.session, "3f1c0d2a-9b7e-4c55-8f1a-2d6e7b9c0a11");
    assert!(
        reading.message.contains("next candidate"),
        "and the reading says where it went: {}",
        reading.message
    );
    assert!(
        reading.message.contains("seams.toml"),
        "{}",
        reading.message
    );
    assert_eq!(reading.key().len(), 40);
    assert!(
        Store::of_workspace(&root)
            .read("permit", "permit.unjudged-call")
            .unwrap()
            .is_none(),
        "nothing reached the end of the walk unjudged"
    );
}

/// The outage of #16 was the loud staleness: a manifest no build reads at
/// all. It exited without judging, which the fallback walks, and every
/// candidate walked to did the same, so the call proceeded unjudged and
/// `status --section permits` said the judge was up for the hour it was
/// down. That one stays a failure, since no rebuild repairs it (#118). The
/// manifest here is a fixture workspace's, the one place a build standing
/// over the checkout it was compiled against still meets rules it cannot
/// read (#57).
#[test]
fn a_manifest_no_candidate_can_read_is_a_failure_the_build_records() {
    let root = scratch("unjudged");
    let nested = root.join("xtask/fixtures/unreadable");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(
        nested.join("seams.toml"),
        "[oracle]\npin = \"oracle.toml\"\n\n[a_table_of_a_later_revision]\ndays = 14\n",
    )
    .unwrap();
    let stdin = serde_json::json!({
        "session_id": "3f1c0d2a-9b7e-4c55-8f1a-2d6e7b9c0a11",
        "tool_name": "Write",
        "tool_input": {
            "file_path": "xtask/fixtures/unreadable/one.rs",
            "content": "fn one() {}\n"
        }
    })
    .to_string();
    let (code, out, err) = permit(&root, &stdin, &[]);
    assert_ne!(code, 0, "the build that cannot judge exits non-zero: {out}");
    assert!(out.trim().is_empty(), "and decides nothing: {out}");
    assert!(err.contains("seams.toml"), "{err}");
    let reading = Store::of_workspace(&root)
        .read("permit", "permit.unjudged-call")
        .unwrap()
        .expect("the refusal was recorded");
    assert_eq!(reading.rule, "permit.unjudged-call");
    assert_eq!(reading.status, Status::Fail);
    assert_eq!(reading.session, "3f1c0d2a-9b7e-4c55-8f1a-2d6e7b9c0a11");
    assert!(
        Store::of_workspace(&root)
            .read("permit", "permit.stale-build")
            .unwrap()
            .is_none(),
        "this build stands over the manifest it was compiled against"
    );
}

/// A denial is the other half of #118 and it does not move: a call a rule
/// refuses is a failing reading under that rule, with the call and the
/// rule's text on it, whatever the refusals beside it read as.
#[test]
fn a_denial_is_still_a_failing_reading_under_the_rule_that_denied_it() {
    let root = scratch("denial-reading");
    let stdin = std::fs::read_to_string(fixtures().join("hooks/pretooluse-deny.json")).unwrap();
    let (code, out, err) = permit(&root, &stdin, &[]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("harness.self"), "{out}");
    let reading = Store::of_workspace(&root)
        .read("permit", "harness.self")
        .unwrap()
        .expect("the denial was recorded");
    assert_eq!(reading.status, Status::Fail);
    assert_eq!(reading.rule, "harness.self");
    assert_eq!(reading.session, "3f1c0d2a-9b7e-4c55-8f1a-2d6e7b9c0a11");
}

/// A build standing over the manifest it was compiled against judges as it
/// did: the sensor fires on staleness and on nothing else.
#[test]
fn a_build_over_the_manifest_it_was_compiled_against_judges_as_it_did() {
    let root = scratch("stale-control");
    let stdin = std::fs::read_to_string(fixtures().join("hooks/pretooluse-deny.json")).unwrap();
    let (code, out, err) = permit(&root, &stdin, &[]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("harness.self"), "{out}");
    for rule in ["permit.stale-build", "permit.unjudged-call"] {
        assert!(
            Store::of_workspace(&root)
                .read("permit", rule)
                .unwrap()
                .is_none(),
            "nothing was refused: {rule}"
        );
    }
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

/// The manifest with the two `ticket.close*` rows #28 proposes, as the
/// tracked file carries them or as the constructors build them.
fn with_the_ticket_close_rules(mut manifest: Manifest) -> Manifest {
    if manifest.rule("ticket.close").is_none() {
        let mut rule = Rule::named("ticket.close", "");
        rule.scope = OneOrMany::None;
        rule.tool = OneOrMany::One("Bash".to_owned());
        rule.predicate.deny = None;
        rule.predicate.deny_argv = Some(vec!["close".to_owned(), "reopen".to_owned()]);
        rule.predicate.with = OneOrMany::One("gh issue".to_owned());
        "A ticket closes through `xtask ticket close`, the instrument that read its acceptance criteria; `gh issue close` and `gh issue reopen` are not available to agents."
            .clone_into(&mut rule.text);
        manifest.rules.push(rule);
    }
    if manifest.rule("ticket.close-api").is_none() {
        let mut rule = Rule::named("ticket.close-api", "");
        rule.scope = OneOrMany::None;
        rule.tool = OneOrMany::One("Bash".to_owned());
        rule.predicate.deny = None;
        rule.predicate.deny_pattern = Some(
            r#"gh\s+api\b[^|;&]*([\s'"]state=(closed|open)\b|"state"\s*:\s*"(closed|open)"|--input\b|(close|reopen)Issue\()"#.to_owned(),
        );
        "The same act through `gh api`: an issue's state is set by `xtask ticket close` and by nothing else."
            .clone_into(&mut rule.text);
        manifest.rules.push(rule);
    }
    manifest
}

// ------------------------------------------------------- the `unless` sensor

/// The rule `unless` qualifies, and the crate its fixture writes into.
const EXIT_AUTHORED: &str = "witness.exit-authored";
const CRATE: &str = "ext/ctype";
const FILE: &str = "ext/ctype/src/lib.rs";

/// The `Write` that would put `text` at `path`.
fn write_call(path: &str, text: &str) -> Call {
    Call::parse(
        &serde_json::json!({
            "tool_name": "Write",
            "tool_input": {"file_path": path, "content": text},
        })
        .to_string(),
    )
    .unwrap()
}

/// A rustdoc block whose snippet exits, its `expect` fence pinning
/// `status`: `exit=3` as `--accept` captures it, `exit` as the opt-in
/// stands before it.
fn block(status: &str) -> String {
    format!(
        "/// ```php\n/// exit(3);\n/// ```\n/// ```expect {status}\n/// ```\npub fn native() {{}}\n"
    )
}

/// The crate's sources: its `Cargo.toml`, which is what says the crate
/// exists at all, and the rustdoc block under test.
fn lay_down(root: &Path, text: &str) {
    std::fs::create_dir_all(root.join(FILE).parent().unwrap()).unwrap();
    std::fs::write(
        root.join(CRATE).join("Cargo.toml"),
        "[package]\nname = \"php_ext_ctype\"\nedition = \"2024\"\n",
    )
    .unwrap();
    std::fs::write(root.join(FILE), text).unwrap();
}

/// The reading `xtask witness` writes over the crate, stamped over its
/// sources as they stand now.
fn witness_reading(root: &Path) {
    let mut reading = Reading::stamped_over(
        "witness",
        CRATE,
        &xtask::key::Snapshot::disk(root).unwrap(),
        Path::new(CRATE),
    )
    .unwrap();
    reading.status = Status::Pass;
    reading.message = format!("{CRATE}: measured");
    Store::of_workspace(root).record(&[reading]).unwrap();
}

fn denial(root: &Path, manifest: &Manifest, call: &Call) -> Option<String> {
    judge(manifest, root, call)
        .unwrap()
        .denial()
        .map(xtask::permit::Finding::reason)
}

/// The `Edit` that would put `new` where `old` stands in `path`.
fn edit_call(path: &str, old: &str, new: &str) -> Call {
    Call::parse(
        &serde_json::json!({
            "tool_name": "Edit",
            "tool_input": {"file_path": path, "old_string": old, "new_string": new},
        })
        .to_string(),
    )
    .unwrap()
}

/// What a call at the hook is measured against is the file it would change
/// (#510): a write that adds an `exit=` is denied whatever the store holds,
/// since the authoring the row refuses happens here, and one that leaves
/// every pinned verdict where it stood adds none and is permitted with no
/// reading behind it. The count decides and not the words, so a second copy
/// of the very line the file carries is a line added. The evidence `unless`
/// reads is the diff judge's question, held by
/// [`what_accept_wrote_permit_diff_permits_over_the_same_tree`].
#[test]
fn a_write_that_adds_an_exit_is_denied_and_one_that_adds_none_is_permitted() {
    let root = scratch("unless");
    let manifest = Manifest::load(&root).unwrap();
    let captured = block("exit=3");
    lay_down(&root, &captured);
    let write = write_call(FILE, &captured);

    // The file's own content: nothing is authored, and no reading is
    // needed to say so.
    assert_eq!(
        denial(&root, &manifest, &write),
        None,
        "a write that leaves the pinned verdict where it stood"
    );
    witness_reading(&root);
    assert_eq!(denial(&root, &manifest, &write), None);

    // The call the rule is for: a status this file does not carry.
    let authored = block("exit=7");
    let denied = denial(&root, &manifest, &write_call(FILE, &authored))
        .expect("an authored status the file has not");
    assert!(denied.contains(EXIT_AUTHORED), "{denied}");

    // One more copy of the line the file already carries is one more all
    // the same, which a fresh reading over the crate does not excuse.
    let denied = denial(
        &root,
        &manifest,
        &write_call(FILE, &format!("{captured}{captured}")),
    )
    .expect("a second copy of the pinned line is a line added");
    assert!(denied.contains(EXIT_AUTHORED), "{denied}");

    // The shape #510 named: an edit beside a pinned verdict, in a file
    // that carries one, adding another. The status is built from the
    // number, as `block` builds it, so this file carries no fence line of
    // its own for the row to read.
    let status = 3;
    let fence = format!("/// ```expect exit={status}");
    let beside = edit_call(
        FILE,
        "pub fn native() {}",
        &format!("{fence}\npub fn native() {{}}"),
    );
    let denied = denial(&root, &manifest, &beside).expect("the edit brings a second verdict");
    assert!(denied.contains(EXIT_AUTHORED), "{denied}");

    // And an edit that moves the prose around the pinned verdict adds
    // nothing, which is the edit the row used to refuse.
    let around = edit_call(FILE, &fence, &format!("/// A note:\n{fence}"));
    assert_eq!(denial(&root, &manifest, &around), None);
}

/// A `deny_pattern` over a write is asked of the file the write leaves and
/// not of the fragment it types (#2413): an edit whose `new_string` holds
/// no hit of its own can still finish one, a digit changed after `exit=`,
/// and that is a status authored. A pattern over a block, which the
/// fragment of an edit inside the block never holds whole, rests on the
/// same reading.
#[test]
fn an_edit_that_finishes_a_hit_outside_its_own_text_is_denied() {
    let root = scratch("unless-fragment");
    let manifest = Manifest::load(&root).unwrap();
    lay_down(&root, &block("exit=3"));
    let finished = edit_call(FILE, "t=3\n", "t=4\n");
    let denied = denial(&root, &manifest, &finished).expect("exit=4 is authored");
    assert!(denied.contains(EXIT_AUTHORED), "{denied}");
    let prose = edit_call(FILE, "pub fn native", "pub fn other");
    assert_eq!(denial(&root, &manifest, &prose), None);
}

/// The evidence this cannot read is no evidence: a crate the manifest does
/// not name has no reading to stand on, so a write that adds the line to a
/// file there is denied however the tree looks.
#[test]
fn an_exit_in_a_file_no_crate_row_names_is_denied() {
    let root = scratch("unless-uncrated");
    let manifest = Manifest::load(&root).unwrap();
    let text = block("exit=3");
    let file = "vendor/other/src/lib.rs";
    std::fs::create_dir_all(root.join(file).parent().unwrap()).unwrap();
    std::fs::write(root.join(file), "pub fn native() {}\n").unwrap();
    let denied = denial(&root, &manifest, &write_call(file, &text)).expect("no crate, no reading");
    assert!(denied.contains(EXIT_AUTHORED), "{denied}");
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

/// The tracked pin's binary, when it is on this machine; a failure where
/// this machine promised the binary (#38).
fn oracle_is_here() -> bool {
    let pin = Manifest::load(workspace()).unwrap().pin;
    Promise::Oracle
        .or_skip(pin.oracle.file().map(Path::to_path_buf))
        .unwrap()
        .is_some()
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

    let add = Command::new("git")
        .args(["add", "--", CRATE])
        .current_dir(&root)
        .status()
        .unwrap();
    assert!(add.success());
    let (code, out, err) = xtask(&root, &["permit", "--diff"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(!out.contains("deny "), "{out}");

    std::fs::remove_file(Store::of_workspace(&root).path("witness", CRATE).unwrap()).unwrap();
    let (code, out, err) = xtask(&root, &["permit", "--diff"]);
    assert_eq!(code, 1, "{out}{err}");
    assert!(
        out.contains("deny ") && out.contains(EXIT_AUTHORED) && out.contains(FILE),
        "{out}"
    );
}

/// A merge commit displaces both its parents (#1312): a verdict the merged
/// branch's `--accept` captured is the branch's, so `permit --diff` over
/// the staged merge permits it with no witness reading over the merged
/// tree, and a verdict the merge itself adds is still denied.
#[test]
fn a_verdict_the_merged_branch_carried_is_the_branchs_and_not_the_merges() {
    let root = scratch("merge-verdict");
    let git = |args: &[&str]| {
        let unhooked = [
            "git",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/var/empty",
        ];
        sh(&root, &[&unhooked[..], args].concat());
    };
    let tail = "pub fn other() {}\n";
    lay_down(&root, &format!("{}{tail}", block("exit")));
    git(&["add", "--", CRATE]);
    git(&["commit", "-q", "-m", "the fence"]);
    git(&["checkout", "-q", "-b", "branch"]);
    write(&root, FILE, &format!("{}{tail}", block("exit=3")));
    git(&["commit", "-q", "-am", "the branch's accept"]);
    git(&["checkout", "-q", "-"]);
    write(
        &root,
        FILE,
        &format!("{}{tail}pub fn mains() {{}}\n", block("exit")),
    );
    git(&["commit", "-q", "-am", "main moves on"]);
    git(&["merge", "-q", "--no-ff", "--no-commit", "branch"]);

    let (code, out, err) = xtask(&root, &["permit", "--diff"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(!out.contains("deny "), "{out}");

    let merged = std::fs::read_to_string(root.join(FILE)).unwrap();
    let authored = block("exit=4").replace("native", "another");
    write(&root, FILE, &format!("{merged}{authored}"));
    git(&["add", "--", FILE]);
    let (code, out, err) = xtask(&root, &["permit", "--diff"]);
    assert_eq!(code, 1, "{out}{err}");
    assert!(out.contains(EXIT_AUTHORED), "{out}");
}

/// The evidence travels with the commits (#1448): a verdict a branch's
/// `--accept` captured, and the reading `witness` took over the branch's
/// commit, stand on main after a merge that also moved the crate, so
/// `done` over the merge reports no `witness.exit-authored`; the same
/// reading as a line of the log, where `worktree retire` appends it,
/// serves as the scope's file does. A verdict typed by hand on another
/// branch, whose commit nothing measured, is still reported. The control:
/// a sensor that reads the scope's file alone reports the captured one,
/// whose stamp is the branch's tree and not the merge's.
#[test]
fn a_verdict_a_branch_captured_stands_on_main_after_the_merge_and_a_typed_one_does_not() {
    let root = scratch("merged-capture");
    let git = |args: &[&str]| {
        let unhooked = [
            "git",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/var/empty",
        ];
        sh(&root, &[&unhooked[..], args].concat());
    };
    let head = || {
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    let reported = |base: &str| {
        let (_, out, err) = xtask(&root, &["done", "--base", base]);
        let said = format!("{out}{err}");
        said.lines()
            .filter(|l| l.contains(EXIT_AUTHORED))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let tail = "\npub fn other() {}\n";
    lay_down(&root, &format!("{}{tail}", block("exit")));
    git(&["add", "--", CRATE]);
    git(&["commit", "-q", "-m", "the fence"]);
    let base = head();

    // The branch captures the verdict and `witness` measures its commit;
    // the reading reaches the store's log alone, as a retire leaves it.
    git(&["checkout", "-q", "-b", "branch"]);
    write(&root, FILE, &format!("{}{tail}", block("exit=3")));
    git(&["commit", "-q", "-am", "the branch's accept"]);
    witness_reading(&root);
    std::fs::remove_file(root.join("target/harness/witness/ext/ctype.toml")).unwrap();
    git(&["checkout", "-q", "-"]);
    write(
        &root,
        FILE,
        &format!("{}{tail}\npub fn mains() {{}}\n", block("exit")),
    );
    git(&["commit", "-q", "-am", "main moves the crate on"]);
    git(&["merge", "-q", "--no-ff", "-m", "merge the branch", "branch"]);
    let merged = std::fs::read_to_string(root.join(FILE)).unwrap();
    assert!(
        merged.contains("exit=3") && merged.contains("mains"),
        "{merged}"
    );
    assert_eq!(reported(&base), Vec::<String>::new());

    // A verdict typed by hand on a second branch, in a commit nothing
    // measured, merged the same way.
    git(&["checkout", "-q", "-b", "typed"]);
    let typed = block("exit=4").replace("native", "another");
    write(&root, FILE, &format!("{merged}\n{typed}"));
    git(&["commit", "-q", "-am", "a verdict typed by hand"]);
    git(&["checkout", "-q", "-"]);
    git(&[
        "merge",
        "-q",
        "--no-ff",
        "-m",
        "merge the typed one",
        "typed",
    ]);
    let lines = reported(&base);
    assert!(
        lines.iter().any(|l| l.contains(FILE)),
        "the typed verdict is reported: {lines:?}"
    );
}

/// The binary over the scratch root, with no stdin.
fn xtask(root: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(xtask::scratch::xtask(env!("CARGO_BIN_EXE_xtask")))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

// ------------------------------------------ `where` and `unless_pattern`

/// A scratch checkout whose branch records `ticket` as `xtask worktree
/// add --ticket` writes it, or records none, as main's checkout.
fn checkout(name: &str, ticket: Option<u32>) -> Tree {
    let dir = scratch(name);
    if let Some(ticket) = ticket {
        let head = Command::new("git")
            .args(["symbolic-ref", "--short", "HEAD"])
            .current_dir(&dir)
            .output()
            .unwrap();
        let key = format!(
            "branch.{}.ticket",
            String::from_utf8_lossy(&head.stdout).trim()
        );
        let set = Command::new("git")
            .args(["config", &key, &ticket.to_string()])
            .current_dir(&dir)
            .status()
            .unwrap();
        assert!(set.success(), "git config {key}");
    }
    dir
}

/// A scratch checkout with a merge in progress, as git leaves one stopped
/// before its commit: `MERGE_HEAD` in its git dir.
fn operating(name: &str) -> Tree {
    let dir = checkout(name, None);
    let git_dir = Command::new("git")
        .args(["rev-parse", "--absolute-git-dir"])
        .current_dir(&dir)
        .output()
        .unwrap();
    let git_dir = PathBuf::from(String::from_utf8_lossy(&git_dir.stdout).trim());
    std::fs::write(git_dir.join("MERGE_HEAD"), "0".repeat(40) + "\n").unwrap();
    dir
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

/// A `Bash` rule denying `xtask phpt`, with its qualifiers set by `with`.
fn probe(with: impl FnOnce(&mut Rule)) -> Manifest {
    let mut manifest = Manifest::load(workspace()).unwrap();
    let mut rule = Rule::named("probe.phpt", "");
    rule.scope = OneOrMany::None;
    rule.tool = OneOrMany::One("Bash".to_owned());
    rule.predicate.deny = None;
    rule.predicate.deny_pattern = Some(r"xtask\s+phpt\b".to_owned());
    with(&mut rule);
    manifest.rules.push(rule);
    manifest
}

fn denied(manifest: &Manifest, root: &Path, line: &str) -> bool {
    let judged = judge(manifest, root, &bash(line)).unwrap();
    judged
        .findings
        .iter()
        .any(|f| f.rule == "probe.phpt" && f.kind == Kind::Deny)
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
    let text = std::fs::read_to_string(workspace().join("seams.toml")).unwrap()
        + "\n[[rule]]\nid = \"orphan\"\ntool = \"Bash\"\ndeny = true\nunless_pattern = \"x\"\ntext = \"t\"\n";
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
/// asks the tracker, and an `allow-starter*` call passes an operation its
/// session started, which a walk does not record: each is held by its
/// own test.
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
        for name in names.iter().filter(|n| !n.starts_with("allow-starter")) {
            let root = match rule.within {
                Where::Ticketed if name.starts_with("allow-main") => main.path(),
                Where::Ticketed => ticketed.path(),
                Where::Anywhere => workspace(),
                Where::Operating => merging.path(),
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
